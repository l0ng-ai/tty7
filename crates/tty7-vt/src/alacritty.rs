//! The alacritty backend: the seam's first engine, wrapping
//! `alacritty_terminal::Term` behind [`VtTerm`].
//!
//! This is the backend everything runs on today — which is why it is the
//! default, compiled with no features and no native code, so CI and
//! contributor builds stay exactly as they were. The Ghostty backend
//! (crate feature `ghostty`) implements the same surface in Phase 3, and
//! consumers flip over without touching their code.
//!
//! Two shapes, one surface:
//!
//! - [`VtTerm`] *owns* its engine state, for consumers that create their
//!   own terminal from a byte stream (the gateway's mirror, the CLI).
//! - [`VtTermSession`] *borrows* a live `Term` for the duration of a call,
//!   for consumers that own their engine state somewhere else already (the
//!   GUI's reader loop holds `Arc<FairMutex<Term>>`); it adds no state and
//!   frees nothing.
//!
//! Conversions between engine types and [`crate::model`] live here and
//! only here. A consumer naming `alacritty_terminal` is one the migration
//! has not reached.

use alacritty_terminal::event::{EventListener, VoidListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell as AlacCell, Flags as AlacFlags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color as AlacColor, CursorShape as AlacCursorShape, CursorStyle as AlacCursorStyle,
    Handler as _, NamedColor, Processor, Rgb as AlacRgb,
};

use crate::model::{
    self, Cell, CellWidth, Cursor, CursorShape, CursorStyle, Modes, Row, Screen, Snapshot,
    StyleColor, Underline,
};

// ---- Conversions -----------------------------------------------------------

impl From<AlacRgb> for model::Rgb {
    fn from(rgb: AlacRgb) -> Self {
        Self {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        }
    }
}

impl From<model::Rgb> for AlacRgb {
    fn from(rgb: model::Rgb) -> Self {
        Self {
            r: rgb.r,
            g: rgb.g,
            b: rgb.b,
        }
    }
}

/// An engine color becomes the model's three-way: theme default, palette
/// index, or direct RGB. `Named` colors outside the 16 ANSI slots collapse
/// onto the slots they render as (`Dim*` onto 0–7, the foreground/
/// background/cursor specials onto "unset") — exactly the distinction the
/// SGR writer on the other side of the seam has to make.
impl From<AlacColor> for StyleColor {
    fn from(color: AlacColor) -> Self {
        match color {
            AlacColor::Spec(rgb) => Self::Rgb(rgb.into()),
            AlacColor::Indexed(i) => Self::Palette(i),
            AlacColor::Named(NamedColor::Foreground)
            | AlacColor::Named(NamedColor::Background)
            | AlacColor::Named(NamedColor::Cursor)
            | AlacColor::Named(NamedColor::DimForeground)
            | AlacColor::Named(NamedColor::BrightForeground) => Self::None,
            // `as` on the enum is its discriminant: Black=0 .. BrightWhite=15,
            // which is the palette index those names render as.
            AlacColor::Named(named @ (NamedColor::Black
            | NamedColor::Red
            | NamedColor::Green
            | NamedColor::Yellow
            | NamedColor::Blue
            | NamedColor::Magenta
            | NamedColor::Cyan
            | NamedColor::White
            | NamedColor::BrightBlack
            | NamedColor::BrightRed
            | NamedColor::BrightGreen
            | NamedColor::BrightYellow
            | NamedColor::BrightBlue
            | NamedColor::BrightMagenta
            | NamedColor::BrightCyan
            | NamedColor::BrightWhite)) => Self::Palette(named as u8),
            // The dim variants share slots 0–7 with their base color when
            // a theme defines no dim palette — the same thing SGR 2 + the
            // base color produces.
            AlacColor::Named(NamedColor::DimBlack) => Self::Palette(0),
            AlacColor::Named(NamedColor::DimRed) => Self::Palette(1),
            AlacColor::Named(NamedColor::DimGreen) => Self::Palette(2),
            AlacColor::Named(NamedColor::DimYellow) => Self::Palette(3),
            AlacColor::Named(NamedColor::DimBlue) => Self::Palette(4),
            AlacColor::Named(NamedColor::DimMagenta) => Self::Palette(5),
            AlacColor::Named(NamedColor::DimCyan) => Self::Palette(6),
            AlacColor::Named(NamedColor::DimWhite) => Self::Palette(7),
        }
    }
}

impl From<AlacCursorShape> for CursorShape {
    fn from(shape: AlacCursorShape) -> Self {
        match shape {
            AlacCursorShape::Beam => Self::Beam,
            AlacCursorShape::Block => Self::Block,
            AlacCursorShape::Underline => Self::Underline,
            AlacCursorShape::HollowBlock => Self::HollowBlock,
            // The engine's "hidden" is a visibility answer wearing a shape
            // tag; the model keeps visibility on the cursor proper.
            AlacCursorShape::Hidden => Self::Block,
        }
    }
}

impl From<CursorShape> for AlacCursorShape {
    fn from(shape: CursorShape) -> Self {
        match shape {
            CursorShape::Beam => Self::Beam,
            CursorShape::Block => Self::Block,
            CursorShape::Underline => Self::Underline,
            CursorShape::HollowBlock => Self::HollowBlock,
        }
    }
}

impl From<AlacCursorStyle> for CursorStyle {
    fn from(style: AlacCursorStyle) -> Self {
        Self {
            shape: style.shape.into(),
            blinking: style.blinking,
        }
    }
}

impl From<CursorStyle> for AlacCursorStyle {
    fn from(style: CursorStyle) -> Self {
        Self {
            shape: style.shape.into(),
            blinking: style.blinking,
        }
    }
}

/// The mode bits the model knows; anything else the engine tracks is
/// invisible through the seam until a consumer needs it.
impl From<&TermMode> for Modes {
    fn from(mode: &TermMode) -> Self {
        let mut out = Modes::empty();
        let map = |flag, bit, out: &mut Modes| {
            if mode.contains(flag) {
                *out |= bit;
            }
        };
        // DECAWM is LINE_WRAP here; the model calls it WRAPAROUND.
        map(TermMode::LINE_WRAP, Modes::WRAPAROUND, &mut out);
        map(TermMode::APP_CURSOR, Modes::APP_CURSOR, &mut out);
        map(TermMode::APP_KEYPAD, Modes::APP_KEYPAD, &mut out);
        map(TermMode::SHOW_CURSOR, Modes::SHOW_CURSOR, &mut out);
        map(TermMode::ALT_SCREEN, Modes::ALT_SCREEN, &mut out);
        map(TermMode::BRACKETED_PASTE, Modes::BRACKETED_PASTE, &mut out);
        map(TermMode::FOCUS_IN_OUT, Modes::FOCUS_IN_OUT, &mut out);
        map(TermMode::MOUSE_REPORT_CLICK, Modes::MOUSE_REPORT_CLICK, &mut out);
        map(TermMode::MOUSE_DRAG, Modes::MOUSE_DRAG, &mut out);
        map(TermMode::MOUSE_MOTION, Modes::MOUSE_MOTION, &mut out);
        map(TermMode::SGR_MOUSE, Modes::SGR_MOUSE, &mut out);
        map(TermMode::ORIGIN, Modes::ORIGIN, &mut out);
        out
    }
}

/// One engine cell to one model cell: the primary char plus its zerowidth
/// marks become the cell's text, spacer flags become [`CellWidth`], flags
/// become the readable bools.
fn cell_to_model(cell: &AlacCell) -> Cell {
    let flags = cell.flags;
    let wide = if flags.contains(AlacFlags::WIDE_CHAR_SPACER) {
        CellWidth::SpacerTail
    } else if flags.contains(AlacFlags::LEADING_WIDE_CHAR_SPACER) {
        CellWidth::SpacerHead
    } else if flags.contains(AlacFlags::WIDE_CHAR) {
        CellWidth::Wide
    } else {
        CellWidth::Narrow
    };
    let underline = if flags.contains(AlacFlags::DOUBLE_UNDERLINE) {
        Underline::Double
    } else if flags.contains(AlacFlags::UNDERCURL) {
        Underline::Curly
    } else if flags.contains(AlacFlags::DOTTED_UNDERLINE) {
        Underline::Dotted
    } else if flags.contains(AlacFlags::DASHED_UNDERLINE) {
        Underline::Dashed
    } else if flags.contains(AlacFlags::UNDERLINE) {
        Underline::Single
    } else {
        Underline::None
    };
    let mut text = String::with_capacity(1 + 4);
    text.push(cell.c);
    if let Some(marks) = cell.zerowidth() {
        text.extend(marks);
    }
    Cell {
        text,
        wide,
        fg: cell.fg.into(),
        bg: cell.bg.into(),
        // SGR 58 (the underline color) is not one the engine being
        // replaced exposes per cell; the renderer blends by theme rule.
        underline_color: StyleColor::None,
        bold: flags.contains(AlacFlags::BOLD),
        italic: flags.contains(AlacFlags::ITALIC),
        dim: flags.contains(AlacFlags::DIM),
        // The engine tracks no blink flag on cells; SGR 5 is a mode there.
        blink: false,
        inverse: flags.contains(AlacFlags::INVERSE),
        invisible: flags.contains(AlacFlags::HIDDEN),
        strikethrough: flags.contains(AlacFlags::STRIKEOUT),
        overline: false,
        underline,
        // OSC 8 anchors are read off the live grid by the GUI's own link
        // probe today; the snapshot does not duplicate them.
        hyperlink: None,
        semantic: model::Semantic::Output,
    }
}

/// The full grid — scrollback plus screen — as an owned snapshot.
fn snapshot_full<T: EventListener>(term: &Term<T>) -> Snapshot {
    let grid = term.grid();
    let cols = grid.columns();
    let screen_rows = grid.screen_lines();
    let history = grid.history_size();
    let mut rows = Vec::with_capacity(history + screen_rows);
    for line in -(history as i32)..screen_rows as i32 {
        let row = &grid[Line(line)];
        // The engine parks the wrap flag on the row's last cell.
        let wrap = row[Column(cols - 1)].flags.contains(AlacFlags::WRAPLINE);
        rows.push(Row {
            cells: (0..cols).map(|c| cell_to_model(&row[Column(c)])).collect(),
            wrap,
            wrap_continuation: false,
            dirty: false,
        });
    }
    let modes = Modes::from(term.mode());
    let cursor_point = grid.cursor.point;
    Snapshot {
        cols: cols as u16,
        screen_rows: screen_rows as u16,
        history_rows: history,
        cursor: Cursor {
            x: cursor_point.column.0 as u16,
            y: cursor_point.line.0.max(0) as u16,
            visible: modes.contains(Modes::SHOW_CURSOR),
            pending_wrap: false,
            screen: if modes.contains(Modes::ALT_SCREEN) {
                Screen::Alternate
            } else {
                Screen::Primary
            },
        },
        modes,
        hyperlinks: Vec::new(),
        rows,
    }
}

// ---- The owning facade -----------------------------------------------------

/// A terminal that owns its engine state: parser, grid, scrollback. What a
/// consumer that replays bytes (the gateway's mirror, `tty7 capture`)
/// drives directly.
pub struct VtTerm {
    term: Term<VoidListener>,
    parser: Processor,
}

impl VtTerm {
    /// A `cols`×`rows` terminal keeping up to `scrollback_lines` lines of
    /// history. Dimensions below the engine's floor (2×1) are raised to
    /// it, as every current caller clamped for itself.
    pub fn new(cols: u16, rows: u16, scrollback_lines: usize) -> Self {
        Self::with_cursor_default(CursorStyle::default(), cols, rows, scrollback_lines)
    }

    /// [`new`](Self::new), with the cursor style a DECSCUSR reset (`0 q`)
    /// falls back to — what the configured `cursor_style` means to a
    /// consumer that restyles cursors around shell marks.
    pub fn with_cursor_default(
        default: CursorStyle,
        cols: u16,
        rows: u16,
        scrollback_lines: usize,
    ) -> Self {
        let config = Config {
            scrolling_history: scrollback_lines,
            default_cursor_style: default.into(),
            ..Config::default()
        };
        Self {
            term: Term::new(config, &MinSize { cols, rows }, VoidListener),
            parser: Processor::new(),
        }
    }

    /// A borrowed session over this terminal, for the facade methods that
    /// operate on a live engine handle (`command_cursor`'s restyling).
    pub fn session(&mut self) -> VtTermSession<'_> {
        VtTermSession::new(&mut self.term)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(MinSize { cols, rows });
    }

    /// Read output. True when it brought a full-screen program's exit: the
    /// alternate screen was active when the call started and is not now.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let was_alt = self.alt();
        self.parser.advance(&mut self.term, bytes);
        was_alt && !self.alt()
    }

    pub fn alt(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// The whole picture as owned data: history rows, screen rows, cursor,
    /// modes. See [`Snapshot`].
    pub fn snapshot_full(&self) -> Snapshot {
        snapshot_full(&self.term)
    }
}

/// The size shim the engine's constructors ask for. alacritty's index
/// arithmetic panics on a zero-sized grid, and a pane that never reported
/// a size leaves callers guessing — so the floor is applied here, once.
struct MinSize {
    cols: u16,
    rows: u16,
}

impl Dimensions for MinSize {
    fn total_lines(&self) -> usize {
        usize::from(self.rows).max(1)
    }
    fn screen_lines(&self) -> usize {
        usize::from(self.rows).max(1)
    }
    fn columns(&self) -> usize {
        usize::from(self.cols).max(2)
    }
}

// ---- The borrowing facade --------------------------------------------------

/// The engine surface a session needs, erased so consumers can hold a
/// [`VtTermSession`] without naming the engine's listener parameter. The
/// GUI's reader loop owns `Term<EventProxy>` — its type never crosses the
/// seam, and thanks to this it never has to.
trait SessionTerm {
    fn cursor_style_raw(&self) -> AlacCursorStyle;
    fn set_cursor_style_raw(&mut self, style: Option<AlacCursorStyle>);
}

impl<T: EventListener> SessionTerm for Term<T> {
    fn cursor_style_raw(&self) -> AlacCursorStyle {
        self.cursor_style()
    }
    fn set_cursor_style_raw(&mut self, style: Option<AlacCursorStyle>) {
        self.set_cursor_style(style);
    }
}

/// A borrowed view over a live engine terminal, for callers that own their
/// `Term` somewhere else (the GUI's `Arc<FairMutex<Term>>`). Adds no state;
/// every method forwards. Constructed with [`VtTermSession::new`] at the
/// call site and dropped right after.
pub struct VtTermSession<'a> {
    term: &'a mut dyn SessionTerm,
}

impl<'a> VtTermSession<'a> {
    /// Over any live engine terminal, whatever event listener it reports
    /// through.
    pub fn new<T: EventListener>(term: &'a mut Term<T>) -> Self {
        Self { term }
    }

    pub fn cursor_style(&self) -> CursorStyle {
        self.term.cursor_style_raw().into()
    }

    /// Restyle the cursor, or with `None` hand it back to the configured
    /// default (what DECSCUSR `0 q` means).
    pub fn set_cursor_style(&mut self, style: Option<CursorStyle>) {
        self.term.set_cursor_style_raw(style.map(Into::into));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(cols: u16, rows: u16, bytes: &[u8]) -> VtTerm {
        let mut term = VtTerm::new(cols, rows, 100);
        term.feed(bytes);
        term
    }

    #[test]
    fn a_snapshot_reads_back_the_text_that_was_written() {
        let term = fed(20, 4, b"one\r\ntwo\r\n\x1b[1;31mred\x1b[0m three");
        let snap = term.snapshot_full();
        assert_eq!(snap.cols, 20);
        assert_eq!(snap.screen_rows, 4);
        let screen: Vec<String> = snap
            .screen()
            .iter()
            .map(|r| {
                r.cells
                    .iter()
                    .filter(|c| c.wide == CellWidth::Narrow || c.wide == CellWidth::Wide)
                    .map(|c| c.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        assert_eq!(screen[..3], ["one", "two", "red three"]);
        let red = &snap.screen()[2].cells[0];
        assert!(red.bold);
        assert_eq!(red.fg, StyleColor::Palette(1));
    }

    #[test]
    fn history_precedes_the_screen_and_wraps_are_marked() {
        // 25 letters through a 10×3 grid: two wrapped rows in, one wraps
        // out into history before the prompt lands.
        let term = fed(10, 3, b"abcdefghijklmnopqrstuvwxy\r\n$ ");
        let snap = term.snapshot_full();
        assert!(snap.history_rows >= 1, "the long line scrolled off");
        let joined: String = snap
            .rows
            .iter()
            .map(|r| {
                r.cells
                    .iter()
                    .filter(|c| c.wide != CellWidth::SpacerTail && c.wide != CellWidth::SpacerHead)
                    .map(|c| c.text.as_str())
                    .collect::<String>()
            })
            .collect();
        assert!(joined.contains("abcdefghijklmnopqrstuvwxy"));
        assert!(snap.history()[0].wrap, "the overflow row is marked wrapping");
    }

    #[test]
    fn colors_collapse_the_way_sgr_will_reencode_them() {
        assert_eq!(StyleColor::from(AlacColor::Named(NamedColor::Red)), StyleColor::Palette(1));
        assert_eq!(
            StyleColor::from(AlacColor::Named(NamedColor::BrightBlue)),
            StyleColor::Palette(12)
        );
        assert_eq!(StyleColor::from(AlacColor::Named(NamedColor::DimCyan)), StyleColor::Palette(6));
        assert_eq!(
            StyleColor::from(AlacColor::Named(NamedColor::Foreground)),
            StyleColor::None
        );
        assert_eq!(StyleColor::from(AlacColor::Indexed(99)), StyleColor::Palette(99));
        assert_eq!(
            StyleColor::from(AlacColor::Spec(AlacRgb { r: 1, g: 2, b: 3 })),
            StyleColor::Rgb(model::Rgb { r: 1, g: 2, b: 3 })
        );
    }

    #[test]
    fn a_cursor_style_round_trips_through_the_model() {
        let mut term = VtTerm::new(10, 3, 10);
        let mut session = term.session();
        assert_eq!(session.cursor_style(), CursorStyle::default());
        session.set_cursor_style(Some(CursorStyle {
            shape: CursorShape::Underline,
            blinking: true,
        }));
        assert_eq!(
            session.cursor_style(),
            CursorStyle { shape: CursorShape::Underline, blinking: true }
        );
        session.set_cursor_style(None);
        assert_eq!(session.cursor_style(), CursorStyle::default());
    }

    #[test]
    fn the_alternate_screen_has_no_history_rows() {
        let mut term = VtTerm::new(20, 4, 100);
        term.feed(b"before\r\n\x1b[?1049h\x1b[2J\x1b[1;1Halt");
        assert!(term.alt());
        let snap = term.snapshot_full();
        assert_eq!(snap.history_rows, 0);
        assert!(snap.modes.contains(Modes::ALT_SCREEN));
        assert_eq!(snap.cursor.screen, Screen::Alternate);
    }
}

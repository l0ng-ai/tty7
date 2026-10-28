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
use alacritty_terminal::index::{Column, Line, Point, Side as AlacSide};
use alacritty_terminal::selection::{Selection as AlacSelection, SelectionType as AlacSelectionType};
use alacritty_terminal::term::cell::{Cell as AlacCell, Flags as AlacFlags};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    ClearMode, Color as AlacColor, CursorShape as AlacCursorShape, CursorStyle as AlacCursorStyle,
    Handler as _, NamedColor, Processor, Rgb as AlacRgb,
};

use crate::model::{
    self, Cell, CellWidth, Cursor, CursorShape, CursorStyle, Glyph, GridPoint, Modes, Row,
    Screen, Selection, SelectionKind, SelectionSpan, Side, Snapshot, StyleColor, Underline,
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

impl From<AlacSelectionType> for SelectionKind {
    fn from(ty: AlacSelectionType) -> Self {
        match ty {
            AlacSelectionType::Simple => Self::Simple,
            AlacSelectionType::Semantic => Self::Semantic,
            AlacSelectionType::Lines => Self::Lines,
            AlacSelectionType::Block => Self::Block,
        }
    }
}

impl From<SelectionKind> for AlacSelectionType {
    fn from(kind: SelectionKind) -> Self {
        match kind {
            SelectionKind::Simple => Self::Simple,
            SelectionKind::Semantic => Self::Semantic,
            SelectionKind::Lines => Self::Lines,
            SelectionKind::Block => Self::Block,
        }
    }
}

impl From<Point> for GridPoint {
    fn from(point: Point) -> Self {
        Self {
            line: point.line.0,
            col: point.column.0 as u16,
        }
    }
}

/// A model point back into the engine's own indexing (the negative lines
/// that address scrollback included). Private: through the seam, points
/// only travel engine → model.
fn to_engine_point(point: GridPoint) -> Point {
    Point::new(Line(point.line), Column(usize::from(point.col)))
}

impl From<AlacSide> for Side {
    fn from(side: AlacSide) -> Self {
        match side {
            AlacSide::Left => Self::Left,
            AlacSide::Right => Self::Right,
        }
    }
}

impl From<Side> for AlacSide {
    fn from(side: Side) -> Self {
        match side {
            Side::Left => Self::Left,
            Side::Right => Self::Right,
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

/// A cell's spacer flags to its width class. The order is the meaning: a
/// spacer tail also carries the wide flag, and a leading spacer is what a
/// soft wrap left holding a wide character back.
fn width_of(flags: AlacFlags) -> CellWidth {
    if flags.contains(AlacFlags::WIDE_CHAR_SPACER) {
        CellWidth::SpacerTail
    } else if flags.contains(AlacFlags::LEADING_WIDE_CHAR_SPACER) {
        CellWidth::SpacerHead
    } else if flags.contains(AlacFlags::WIDE_CHAR) {
        CellWidth::Wide
    } else {
        CellWidth::Narrow
    }
}

/// One engine cell to the walker's view of it ([`Glyph`]).
fn glyph_of(cell: &AlacCell) -> Glyph {
    Glyph {
        c: cell.c,
        wide: width_of(cell.flags),
        hyperlink: cell.hyperlink().map(|h| h.uri().to_string()),
    }
}

/// One engine cell to one model cell: the primary char plus its zerowidth
/// marks become the cell's text, spacer flags become [`CellWidth`], flags
/// become the readable bools.
fn cell_to_model(cell: &AlacCell) -> Cell {
    let flags = cell.flags;
    let wide = width_of(flags);
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
    /// history. A zero dimension is raised to 1 — the one size the engine
    /// cannot hold; everything wider is built exactly as asked (see
    /// [`MinSize`]).
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

    /// The whole grid — scrollback plus screen — as plain text, wrapped
    /// rows joined, exactly as the capture path's own `bounds_to_string`
    /// over `topmost_line..=bottommost_line` read it before the facade
    /// existed. The Ghostty backend's `Terminal::text_full` answers the
    /// same question by the same rules.
    pub fn text_full(&self) -> String {
        let start = Point::new(self.term.topmost_line(), Column(0));
        let end = Point::new(self.term.bottommost_line(), self.term.last_column());
        self.term.bounds_to_string(start, end)
    }
}

/// The size shim the engine's constructors and resize ask for. Zero is the
/// one size alacritty cannot hold (its index arithmetic panics on it), and
/// a pane that never reported a size leaves callers guessing — so the floor
/// is applied here, once. The floor is 1×1, not 2×1: neither `Term::new`
/// nor `Term::resize` clamps columns in the engine this replaces, and a
/// 1-column grid is a real (if narrow) terminal it built happily, pinned
/// by the tests below.
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
        usize::from(self.cols).max(1)
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
    fn columns_raw(&self) -> usize;
    fn screen_lines_raw(&self) -> usize;
    fn topmost_line_raw(&self) -> i32;
    fn bottommost_line_raw(&self) -> i32;
    fn glyph_raw(&self, point: GridPoint) -> Glyph;
    fn row_wraps_raw(&self, line: i32) -> bool;
    fn clear_row_wrap_raw(&mut self, line: i32);
    fn cursor_raw(&self) -> Cursor;
    fn set_cursor_raw(&mut self, point: GridPoint, pending_wrap: bool);
    fn text_raw(&self, from: GridPoint, to: GridPoint) -> String;
    fn selection_raw(&self) -> Option<Selection>;
    fn selection_to_string_raw(&self) -> Option<String>;
    fn set_selection_raw(
        &mut self,
        kind: SelectionKind,
        from: GridPoint,
        from_side: Side,
        to: GridPoint,
        to_side: Side,
    );
    fn modes_raw(&self) -> Modes;
    fn semantic_escape_chars_raw(&self) -> &str;
    fn resize_raw(&mut self, cols: usize, rows: usize);
    fn clear_below_raw(&mut self);
    fn linefeed_raw(&mut self);
}

impl<T: EventListener> SessionTerm for Term<T> {
    fn cursor_style_raw(&self) -> AlacCursorStyle {
        self.cursor_style()
    }
    fn set_cursor_style_raw(&mut self, style: Option<AlacCursorStyle>) {
        self.set_cursor_style(style);
    }
    fn columns_raw(&self) -> usize {
        self.columns()
    }
    fn screen_lines_raw(&self) -> usize {
        self.screen_lines()
    }
    fn topmost_line_raw(&self) -> i32 {
        self.topmost_line().0
    }
    fn bottommost_line_raw(&self) -> i32 {
        self.bottommost_line().0
    }
    fn glyph_raw(&self, point: GridPoint) -> Glyph {
        glyph_of(&self.grid()[Line(point.line)][Column(usize::from(point.col))])
    }
    fn row_wraps_raw(&self, line: i32) -> bool {
        let last = Column(self.columns() - 1);
        self.grid()[Line(line)][last].flags.contains(AlacFlags::WRAPLINE)
    }
    fn clear_row_wrap_raw(&mut self, line: i32) {
        let last = Column(self.columns() - 1);
        self.grid_mut()[Line(line)][last].flags.remove(AlacFlags::WRAPLINE);
    }
    fn cursor_raw(&self) -> Cursor {
        let cursor = &self.grid().cursor;
        let modes = Modes::from(self.mode());
        Cursor {
            x: cursor.point.column.0 as u16,
            y: cursor.point.line.0.max(0) as u16,
            visible: modes.contains(Modes::SHOW_CURSOR),
            pending_wrap: cursor.input_needs_wrap,
            screen: if modes.contains(Modes::ALT_SCREEN) {
                Screen::Alternate
            } else {
                Screen::Primary
            },
        }
    }
    fn set_cursor_raw(&mut self, point: GridPoint, pending_wrap: bool) {
        // A position remembered from before a resize must not index
        // outside the grid — the clamp the parked-cursor repair did for
        // itself before it moved onto the facade.
        let line = point.line.clamp(0, self.screen_lines() as i32 - 1);
        let column = usize::from(point.col).min(self.columns().saturating_sub(1));
        let cursor = &mut self.grid_mut().cursor;
        cursor.point = Point::new(Line(line), Column(column));
        cursor.input_needs_wrap = pending_wrap;
    }
    fn text_raw(&self, from: GridPoint, to: GridPoint) -> String {
        self.bounds_to_string(to_engine_point(from), to_engine_point(to))
    }
    fn selection_raw(&self) -> Option<Selection> {
        let selection = self.selection.as_ref()?;
        let range = selection.to_range(self)?;
        Some(Selection {
            kind: selection.ty.into(),
            span: SelectionSpan {
                start: range.start.into(),
                end: range.end.into(),
            },
        })
    }
    fn selection_to_string_raw(&self) -> Option<String> {
        self.selection_to_string()
    }
    fn set_selection_raw(
        &mut self,
        kind: SelectionKind,
        from: GridPoint,
        from_side: Side,
        to: GridPoint,
        to_side: Side,
    ) {
        let mut selection =
            AlacSelection::new(kind.into(), to_engine_point(from), from_side.into());
        selection.update(to_engine_point(to), to_side.into());
        self.selection = Some(selection);
    }
    fn modes_raw(&self) -> Modes {
        Modes::from(self.mode())
    }
    fn semantic_escape_chars_raw(&self) -> &str {
        self.semantic_escape_chars()
    }
    fn resize_raw(&mut self, cols: usize, rows: usize) {
        let clamp = |n: usize| n.clamp(1, usize::from(u16::MAX)) as u16;
        self.resize(MinSize { cols: clamp(cols), rows: clamp(rows) });
    }
    fn clear_below_raw(&mut self) {
        self.clear_screen(ClearMode::Below);
    }
    fn linefeed_raw(&mut self) {
        self.linefeed();
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

    /// Grid width in cells.
    pub fn columns(&self) -> usize {
        self.term.columns_raw()
    }

    /// Active-area height in cells (scrollback not counted).
    pub fn screen_lines(&self) -> usize {
        self.term.screen_lines_raw()
    }

    /// The topmost line addressable by [`GridPoint::line`] — negative, one
    /// per row of scrollback.
    pub fn topmost_line(&self) -> i32 {
        self.term.topmost_line_raw()
    }

    /// The bottom-most screen row's line address.
    pub fn bottommost_line(&self) -> i32 {
        self.term.bottommost_line_raw()
    }

    /// One cell as a walker sees it: primary character, width class, OSC 8
    /// anchor. The point must lie inside the grid, as the walkers that
    /// bound themselves with [`Self::topmost_line`]/[`Self::columns`] do.
    pub fn glyph(&self, point: GridPoint) -> Glyph {
        self.term.glyph_raw(point)
    }

    /// Whether the row soft-wrapped into the next one — no line break
    /// after it.
    pub fn row_wraps(&self, line: i32) -> bool {
        self.term.row_wraps_raw(line)
    }

    /// Turn a soft wrap into a hard break: the row now ends a logical line
    /// where the padding's wrap used to continue it.
    pub fn clear_row_wrap(&mut self, line: i32) {
        self.term.clear_row_wrap_raw(line)
    }

    /// The cursor: position, visibility, pending wrap, which screen.
    pub fn cursor(&self) -> Cursor {
        self.term.cursor_raw()
    }

    /// Put the cursor somewhere, with its pending-wrap state. The position
    /// clamps into the active area — a position remembered from before a
    /// resize must not index outside the grid.
    pub fn set_cursor(&mut self, point: GridPoint, pending_wrap: bool) {
        self.term.set_cursor_raw(point, pending_wrap);
    }

    /// The text between two points, wrapped rows joined, exactly as the
    /// engine's own selection-less extraction read it.
    pub fn text(&self, from: GridPoint, to: GridPoint) -> String {
        self.term.text_raw(from, to)
    }

    /// The active selection: how it was made, and the span it covers.
    /// `None` when there is none, or when it no longer intersects the grid.
    pub fn selection(&self) -> Option<Selection> {
        self.term.selection_raw()
    }

    /// The selection's text, as the engine itself converts it (a `Lines`
    /// selection keeps whole rows and ends in a newline, and so on).
    pub fn selection_to_string(&self) -> Option<String> {
        self.term.selection_to_string_raw()
    }

    /// Replace the active selection with one of the given kind between two
    /// cell edges — what the mouse gestures build on the GUI side.
    pub fn set_selection(
        &mut self,
        kind: SelectionKind,
        from: GridPoint,
        from_side: Side,
        to: GridPoint,
        to_side: Side,
    ) {
        self.term.set_selection_raw(kind, from, from_side, to, to_side);
    }

    /// The mode bits the model knows.
    pub fn modes(&self) -> Modes {
        self.term.modes_raw()
    }

    /// The characters that end a word for selection — the embedder's
    /// configuration, not a constant.
    pub fn semantic_escape_chars(&self) -> &str {
        self.term.semantic_escape_chars_raw()
    }

    /// Resize the grid, reflowing the primary screen. A zero dimension
    /// becomes 1; nothing else is rounded (see [`MinSize`]).
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.term.resize_raw(cols, rows);
    }

    /// Clear from the cursor to the end of the screen (`CSI 0 J`): the
    /// rest of the cursor's row and every row below it.
    pub fn clear_below(&mut self) {
        self.term.clear_below_raw();
    }

    /// Move the cursor down one row, scrolling at the bottom of the scroll
    /// region — what a bare LF does.
    pub fn linefeed(&mut self) {
        self.term.linefeed_raw();
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

    /// Today's capture path, verbatim: a raw engine terminal built and read
    /// the way `tty7 capture --plain` did before the facade. The pin tests
    /// below assert the facade answers identically — literal for literal
    /// and against this, both.
    fn raw_full(cols: u16, rows: u16, bytes: &[u8]) -> String {
        let mut term = Term::new(Config::default(), &MinSize { cols, rows }, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        let start = Point::new(term.topmost_line(), Column(0));
        let end = Point::new(term.bottommost_line(), term.last_column());
        term.bounds_to_string(start, end)
    }

    /// Neither the constructor nor the resize of the engine being replaced
    /// clamps columns, and a 1-column grid is a real terminal it built
    /// happily — so the facade must build one too, byte for byte. The
    /// literals are what the raw engine answered on this exact input.
    #[test]
    fn a_one_column_terminal_reads_as_the_engine_read_it() {
        let mut term = VtTerm::new(1, 4, 10_000);
        term.feed(b"ab\r\ncd\r\n\x1b[1;31mef");
        let text = term.text_full();
        assert_eq!(text, "ab\ncd\nef");
        assert_eq!(text, raw_full(1, 4, b"ab\r\ncd\r\n\x1b[1;31mef"));
    }

    #[test]
    fn a_resize_to_one_column_reflows_as_the_engine_did() {
        let mut term = VtTerm::new(20, 4, 10_000);
        term.feed(b"abcdefghij\r\nkl");
        term.session().resize(1, 4);
        let text = term.text_full();
        assert_eq!(text, "abcdefghij\nkl\n\n");
        // The raw engine's own resize path, for the same bytes.
        let mut raw = Term::new(Config::default(), &MinSize { cols: 20, rows: 4 }, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut raw, b"abcdefghij\r\nkl");
        raw.resize(MinSize { cols: 1, rows: 4 });
        let start = Point::new(raw.topmost_line(), Column(0));
        let end = Point::new(raw.bottommost_line(), raw.last_column());
        assert_eq!(text, raw.bounds_to_string(start, end));
    }

    #[test]
    fn text_full_joins_wrapped_rows_and_trims_one_trailing_newline() {
        let term = fed(10, 3, b"abcdefghij\r\none two");
        assert_eq!(term.text_full(), "abcdefghij\none two\n");
        // A soft-wrapped row contributes no break of its own: eight chars
        // through five columns are two rows, one line.
        let term = fed(5, 3, b"abcdefgh");
        assert_eq!(term.text_full(), "abcdefgh\n");
    }

    #[test]
    fn text_reads_between_two_points() {
        let mut term = fed(10, 3, b"one two\r\nthree");
        let mid = GridPoint { line: 0, col: 4 };
        let end = GridPoint { line: 1, col: 4 };
        assert_eq!(term.session().text(mid, end), "two\nthree");
    }

    #[test]
    fn a_glyph_reads_char_width_and_hyperlink() {
        let mut term = VtTerm::new(20, 3, 10);
        term.feed("run \x1b]8;;https://e.com\x1b\\link\x1b]8;;\x1b\\ 北".as_bytes());
        let plain = term.session().glyph(GridPoint { line: 0, col: 0 });
        assert_eq!(
            (plain.c, plain.wide, plain.hyperlink.as_deref()),
            ('r', CellWidth::Narrow, None)
        );
        let link = term.session().glyph(GridPoint { line: 0, col: 4 });
        assert_eq!((link.c, link.hyperlink.as_deref()), ('l', Some("https://e.com")));
        let wide = term.session().glyph(GridPoint { line: 0, col: 9 });
        assert_eq!((wide.c, wide.wide), ('北', CellWidth::Wide));
        assert_eq!(
            term.session().glyph(GridPoint { line: 0, col: 10 }).wide,
            CellWidth::SpacerTail
        );
    }

    #[test]
    fn the_cursor_round_trips_and_clamps_into_the_grid() {
        let mut term = fed(10, 3, b"hi");
        let mut session = term.session();
        session.set_cursor(GridPoint { line: 1, col: 2 }, true);
        let cursor = session.cursor();
        assert_eq!((cursor.y, cursor.x, cursor.pending_wrap), (1, 2, true));
        // A position from before a resize: outside the grid clamps inside.
        session.set_cursor(GridPoint { line: 90, col: 90 }, false);
        let cursor = session.cursor();
        assert_eq!((cursor.y, cursor.x), (2, 9));
    }

    #[test]
    fn a_row_wrap_reads_and_clears() {
        let mut term = VtTerm::new(5, 3, 10);
        term.feed(b"abcdefgh");
        assert!(term.session().row_wraps(0), "the row soft-wrapped");
        term.session().clear_row_wrap(0);
        assert!(!term.session().row_wraps(0));
        assert_eq!(term.text_full(), "abcde\nfgh\n");
    }

    #[test]
    fn a_selection_reports_kind_span_and_text() {
        let mut term = VtTerm::new(10, 3, 10);
        term.feed(b"one two\r\nthree");
        term.session().set_selection(
            SelectionKind::Simple,
            GridPoint { line: 0, col: 4 },
            Side::Left,
            GridPoint { line: 1, col: 4 },
            Side::Right,
        );
        let selection = term.session().selection().expect("a selection");
        assert_eq!(selection.kind, SelectionKind::Simple);
        assert_eq!(selection.span.start, GridPoint { line: 0, col: 4 });
        assert_eq!(selection.span.end, GridPoint { line: 1, col: 4 });
        assert_eq!(term.session().selection_to_string().as_deref(), Some("two\nthree"));

        // Lines selections span whole rows and convert with a trailing
        // newline; block ones say so.
        term.session().set_selection(
            SelectionKind::Lines,
            GridPoint { line: 0, col: 0 },
            Side::Left,
            GridPoint { line: 1, col: 0 },
            Side::Right,
        );
        assert_eq!(
            term.session().selection_to_string().as_deref(),
            Some("one two\nthree\n")
        );
        term.session().set_selection(
            SelectionKind::Block,
            GridPoint { line: 0, col: 0 },
            Side::Left,
            GridPoint { line: 1, col: 2 },
            Side::Right,
        );
        assert_eq!(term.session().selection().unwrap().kind, SelectionKind::Block);
        assert_eq!(term.session().selection_to_string().as_deref(), Some("one\nthr"));

        // A selection that runs through the cleared rows is dropped by
        // the clear (the grid it selected no longer exists); one entirely
        // above it keeps.
        term.session().set_selection(
            SelectionKind::Simple,
            GridPoint { line: 0, col: 4 },
            Side::Left,
            GridPoint { line: 1, col: 4 },
            Side::Right,
        );
        term.session().clear_below();
        assert_eq!(term.session().selection_to_string(), None);
    }

    #[test]
    fn geometry_reaches_into_scrollback() {
        let mut term = fed(10, 3, b"l1\r\nl2\r\nl3\r\nl4\r\nl5\r\n");
        let session = term.session();
        assert_eq!((session.columns(), session.screen_lines()), (10, 3));
        assert_eq!(session.bottommost_line(), 2);
        // Three rows of the five-line draw scrolled off the three-row
        // screen (the trailing \r\n opens a row of its own).
        assert_eq!(session.topmost_line(), -3);
        assert_eq!(session.glyph(GridPoint { line: -3, col: 0 }).c, 'l');
    }

    #[test]
    fn modes_and_separators_read_off_the_live_terminal() {
        let mut term = VtTerm::new(10, 3, 10);
        assert!(!term.session().modes().contains(Modes::ALT_SCREEN));
        term.feed(b"\x1b[?1049h");
        assert!(term.session().modes().contains(Modes::ALT_SCREEN));
        // The embedder configures separators; the facade carries whatever
        // the terminal holds, here the engine default.
        assert_eq!(term.session().semantic_escape_chars(), ",│`|:\"' ()[]{}<>\t");
    }

    #[test]
    fn clear_below_erases_from_the_cursor_and_linefeed_scrolls() {
        let mut term = fed(10, 4, b"keep\r\none\r\ntwo\r\nthree");
        {
            let mut session = term.session();
            session.set_cursor(GridPoint { line: 1, col: 0 }, false);
            session.clear_below();
        }
        assert_eq!(term.text_full(), "keep\n\n\n");
        let mut session = term.session();
        session.set_cursor(GridPoint { line: 3, col: 0 }, false);
        session.linefeed();
        assert_eq!(session.cursor().y, 3, "the cursor cannot leave the screen");
        assert_eq!(session.topmost_line(), -1, "at the bottom a linefeed scrolls instead");
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

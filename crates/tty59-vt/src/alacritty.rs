//! The alacritty backend: the seam's first engine, wrapping
//! `alacritty_terminal::Term` behind [`VtTerm`].
//!
//! This is the backend everything runs on today — which is why it is the
//! default, compiled with no features and no native code, so CI and
//! contributor builds stay exactly as they were. The Ghostty backend
//! (crate feature `ghostty`) implements the same [`crate::engine`] surface
//! in Phase 3, and consumers flip over without touching their code: under
//! the feature this type keeps compiling (as `tty59_vt::AlacTerm`) so the
//! differential harness can drive both engines side by side.
//!
//! One shape: [`VtTerm`] *owns* its engine state, for consumers that create
//! their own terminal from a byte stream (the gateway's mirror, the CLI) —
//! and, with a listener, for the GUI's reader loop, which holds one behind
//! the seam's own fair mutex (`Arc<FairMutex<VtTerm<Listener>>>`). The
//! borrowing facades ([`crate::VtTermSession`], [`crate::VtTermView`]) are
//! engine-neutral and live in [`crate::engine`].
//!
//! Conversions between engine types and [`crate::model`] live here and
//! only here. A consumer naming `alacritty_terminal` is one the migration
//! has not reached.

use std::sync::Arc;

use alacritty_terminal::event::{
    Event as AlacEvent, EventListener, WindowSize as AlacWindowSize,
};
use alacritty_terminal::grid::{Dimensions, Scroll as AlacScroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point, Side as AlacSide};
use alacritty_terminal::selection::{Selection as AlacSelection, SelectionType as AlacSelectionType};
use alacritty_terminal::term::cell::{Cell as AlacCell, Flags as AlacFlags};
use alacritty_terminal::term::search::RegexSearch;
use alacritty_terminal::term::{ClipboardType as AlacClipboardType, Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    ClearMode, Color as AlacColor, CursorShape as AlacCursorShape, CursorStyle as AlacCursorStyle,
    Handler as _, NamedColor, Processor, Rgb as AlacRgb,
};

use crate::engine::{sealed, SessionRead, SessionTerm, VtTermSession, VtTermView};
use crate::event::{ClipboardType, VtEvent, VtEventListener};
use crate::model::{
    self, Cell, CellWidth, Cursor, CursorShape, CursorStyle, Frame, FrameCursor, Glyph, GridPoint,
    Modes, Row, Screen, Scroll, SearchMatch, SearchQuery, SearchStatus, Selection, SelectionKind,
    SelectionSpan, Side, Snapshot, StyleColor, Underline, VtOptions,
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
        // The kitty keyboard protocol's progressive-enhancement flags. The
        // engine spells them in ESC_CODES/AS_ESC/TEXT; the model names the
        // protocol and the flag, which is what a key encoder needs to ask.
        map(TermMode::DISAMBIGUATE_ESC_CODES, Modes::KITTY_DISAMBIGUATE, &mut out);
        map(TermMode::REPORT_ALL_KEYS_AS_ESC, Modes::KITTY_REPORT_ALL_KEYS, &mut out);
        map(TermMode::REPORT_ASSOCIATED_TEXT, Modes::KITTY_REPORT_TEXT, &mut out);
        map(TermMode::REPORT_EVENT_TYPES, Modes::KITTY_REPORT_EVENT_TYPES, &mut out);
        map(TermMode::REPORT_ALTERNATE_KEYS, Modes::KITTY_REPORT_ALTERNATE_KEYS, &mut out);
        map(TermMode::UTF8_MOUSE, Modes::UTF8_MOUSE, &mut out);
        map(TermMode::ALTERNATE_SCROLL, Modes::ALTERNATE_SCROLL, &mut out);
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

/// The engine's underline flags to the model's enum, in the engine's own
/// precedence order (double beats curly beats dotted beats dashed beats
/// single — the order a renderer stacking them has to resolve by).
fn underline_of(flags: AlacFlags) -> Underline {
    if flags.contains(AlacFlags::DOUBLE_UNDERLINE) {
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
    }
}

/// Give a cell's link its index in the frame's URI side table: one index
/// per distinct URI, so `hyperlink.is_some()` answers "linked" and the
/// table maps an index back to its URI. The previous intern is checked
/// first because links arrive in runs.
fn intern_hyperlink(
    link: Option<alacritty_terminal::term::cell::Hyperlink>,
    hyperlinks: &mut Vec<String>,
) -> Option<u16> {
    let link = link?;
    let uri = link.uri();
    if hyperlinks.last().is_some_and(|u| u == uri) {
        return Some((hyperlinks.len() - 1) as u16);
    }
    match hyperlinks.iter().position(|u| u == uri) {
        Some(i) => Some(i as u16),
        // A screen can hold more distinct links than u16 indexes; past
        // that, overflow to "no link" rather than alias an unrelated URI.
        None if hyperlinks.len() < u16::MAX as usize => {
            hyperlinks.push(uri.to_string());
            Some((hyperlinks.len() - 1) as u16)
        }
        None => None,
    }
}

/// One engine cell into a model cell *in place*: every field overwritten,
/// the marks vector's inline capacity (and nothing else) carried over from
/// the last fill. This is the frame's per-cell cost, so it allocates
/// nothing once the buffer is warm — the price of [`Frame`] being ownable
/// with no lock held.
fn fill_frame_cell(out: &mut Cell, cell: &AlacCell, hyperlinks: &mut Vec<String>) {
    let flags = cell.flags;
    out.wide = width_of(flags);
    out.c = cell.c;
    out.marks.clear();
    if let Some(marks) = cell.zerowidth() {
        out.marks.extend_from_slice(marks);
    }
    out.fg = cell.fg.into();
    out.bg = cell.bg.into();
    // SGR 58 (the underline color) is not one the engine being replaced
    // exposes through the snapshot's projection; the frame carries it.
    out.underline_color = cell.underline_color().map(StyleColor::from).unwrap_or_default();
    out.bold = flags.contains(AlacFlags::BOLD);
    out.italic = flags.contains(AlacFlags::ITALIC);
    out.dim = flags.contains(AlacFlags::DIM);
    // The engine tracks no blink flag on cells; SGR 5 is a mode there.
    out.blink = false;
    out.inverse = flags.contains(AlacFlags::INVERSE);
    out.invisible = flags.contains(AlacFlags::HIDDEN);
    out.strikethrough = flags.contains(AlacFlags::STRIKEOUT);
    out.overline = false;
    out.underline = underline_of(flags);
    out.hyperlink = intern_hyperlink(cell.hyperlink(), hyperlinks);
    out.semantic = model::Semantic::Output;
}

/// One engine cell to one model cell: the primary char plus its zerowidth
/// marks become the cell's text, spacer flags become [`CellWidth`], flags
/// become the readable bools.
fn cell_to_model(cell: &AlacCell) -> Cell {
    let mut out = Cell::default();
    fill_frame_cell(&mut out, cell, &mut Vec::new());
    // The snapshot's projection: links and SGR 58 anchors are read off the
    // live grid by the GUI's own link probe today, so they stay off it.
    out.hyperlink = None;
    out.underline_color = StyleColor::None;
    out
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

/// The engine's view of a model event listener. A newtype adapter because
/// the orphan rule stops this crate from implementing the engine's listener
/// trait for every [`VtEventListener`] at once (E0210: foreign trait,
/// uncovered type parameter): wrapping gives the impl a local type to
/// belong to, and the facade hides the wrapper so a consumer names only
/// `VtTerm<ItsListener>`.
struct SinkListener<S: VtEventListener>(S);

impl<S: VtEventListener> EventListener for SinkListener<S> {
    fn send_event(&self, event: AlacEvent) {
        if let Some(event) = model_event_of(event) {
            self.0.send_event(event);
        }
    }
}

/// One engine event into the model's shape, or `None` for the ones no tty59
/// consumer has ever matched: the engine's mouse-cursor and cursor-blink
/// hints, which the GUI's catch-all arm has dropped for as long as it has
/// matched events. Dropping them at the boundary is the same observable
/// behavior.
fn model_event_of(event: AlacEvent) -> Option<VtEvent> {
    Some(match event {
        AlacEvent::Wakeup => VtEvent::Wakeup,
        AlacEvent::Title(title) => VtEvent::Title(title),
        AlacEvent::ResetTitle => VtEvent::ResetTitle,
        AlacEvent::PtyWrite(bytes) => VtEvent::PtyWrite(bytes.into_bytes()),
        AlacEvent::Exit => VtEvent::Exit,
        AlacEvent::ChildExit(status) => VtEvent::ChildExit(status),
        AlacEvent::ClipboardStore(kind, text) => VtEvent::ClipboardStore(kind.into(), text),
        // The formatter here and the model's are the same trait object; the
        // Arc crosses as is.
        AlacEvent::ClipboardLoad(kind, fmt) => VtEvent::ClipboardLoad(kind.into(), fmt),
        // The reply formatters take engine types, so each is re-wrapped
        // around the model's own — the one conversion at the boundary.
        AlacEvent::ColorRequest(index, fmt) => {
            VtEvent::ColorRequest(index, Arc::new(move |rgb| fmt(AlacRgb::from(rgb))))
        }
        AlacEvent::TextAreaSizeRequest(fmt) => VtEvent::TextAreaSizeRequest(Arc::new(move |ws| {
            fmt(AlacWindowSize {
                num_lines: ws.rows,
                num_cols: ws.cols,
                cell_width: ws.cell_width,
                cell_height: ws.cell_height,
            })
        })),
        AlacEvent::Bell => VtEvent::Bell,
        AlacEvent::MouseCursorDirty | AlacEvent::CursorBlinkingChange => return None,
    })
}

impl From<AlacClipboardType> for ClipboardType {
    fn from(kind: AlacClipboardType) -> Self {
        match kind {
            AlacClipboardType::Clipboard => Self::Clipboard,
            AlacClipboardType::Selection => Self::Selection,
        }
    }
}

/// The model's options into the engine's own `Config` — every option the
/// seam names, mapped; a new engine field without a model name is a gap,
/// not a default.
fn config_of(options: &VtOptions) -> Config {
    Config {
        scrolling_history: options.scrollback_lines,
        default_cursor_style: options.cursor_style.into(),
        semantic_escape_chars: options.semantic_escape_chars.clone(),
        kitty_keyboard: options.kitty_keyboard,
        conpty_resize: options.conpty_resize,
        ..Config::default()
    }
}

/// A terminal that owns its engine state: parser, grid, scrollback, and the
/// listener its effects go to. What a consumer that replays bytes (the
/// gateway's mirror, `tty59 capture`) drives directly — and, with a real
/// listener, what the GUI's reader thread owns behind the seam's fair
/// mutex.
pub struct VtTerm<S: VtEventListener = ()> {
    term: Term<SinkListener<S>>,
    parser: Processor,
}

impl VtTerm<()> {
    /// A `cols`×`rows` terminal keeping up to `scrollback_lines` lines of
    /// history, effects dropped. A zero dimension is raised to 1 — the one
    /// size the engine cannot hold; everything wider is built exactly as
    /// asked (see [`MinSize`]).
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
        Self::with_options(
            VtOptions {
                scrollback_lines,
                cursor_style: default,
                ..VtOptions::default()
            },
            cols,
            rows,
        )
    }

    /// [`with_listener`](Self::with_listener) with the no-op listener:
    /// parse only, observe nothing.
    pub fn with_options(options: VtOptions, cols: u16, rows: u16) -> Self {
        Self::with_listener(options, cols, rows, ())
    }
}

impl<S: VtEventListener> VtTerm<S> {
    /// A `cols`×`rows` terminal configured from model options, reporting
    /// its effects to `listener` as model [`VtEvent`]s.
    pub fn with_listener(options: VtOptions, cols: u16, rows: u16, listener: S) -> Self {
        Self {
            term: Term::new(config_of(&options), &MinSize { cols, rows }, SinkListener(listener)),
            parser: Processor::new(),
        }
    }

    /// A borrowed session over this terminal, for the facade methods that
    /// operate on a live engine handle (`command_cursor`'s restyling).
    pub fn session(&mut self) -> VtTermSession<'_> {
        VtTermSession::new(&mut self.term)
    }

    /// A borrowed read-only view over this terminal, for the facade's
    /// readers (tests walking a fixture grid; a consumer that only reads).
    pub fn view(&self) -> VtTermView<'_> {
        VtTermView::new(&self.term)
    }

    /// Parse `bytes` into the engine: the reader loop's hot path, one call
    /// per read. A thin forward to the parser over the term — the seam must
    /// not add work here (the Phase 2e perf gate pins it to the raw
    /// `Processor::advance` within ~2%).
    #[inline]
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// While a synchronized update (DEC mode 2026) is held open, the
    /// deadline after which it is flushed — the reader parks its read on
    /// this.
    pub fn sync_timeout(&self) -> Option<std::time::Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Flush a synchronized update ahead of its deadline.
    pub fn stop_sync(&mut self) {
        self.parser.stop_sync(&mut self.term);
    }

    /// Tell the pane to shut down (the engine raises an `Exit`).
    pub fn exit(&mut self) {
        self.term.exit();
    }

    /// Clear both screens' kitty keyboard flag stacks — for an embedder
    /// that knows the program which negotiated them is gone (fork patch
    /// #4: no escape sequence can clear the stacks, only this can).
    pub fn reset_keyboard_modes(&mut self) {
        self.term.reset_keyboard_modes();
    }

    /// Reset the emulator to a fresh grid, title and all — what a relink
    /// does before the daemon replays the pane from scratch.
    pub fn reset_state(&mut self) {
        use alacritty_terminal::vte::ansi::Handler as _;
        self.term.reset_state();
    }

    /// Apply new options (the user reconfigured their terminal).
    pub fn set_options(&mut self, options: VtOptions) {
        self.term.set_options(config_of(&options));
    }

    /// Whether this engine answers color queries from configured options —
    /// the seam's color-push question. False here: the engine's color
    /// replies flow through [`VtEvent::ColorRequest`] at query time, so a
    /// theme push would be a options round-trip for nothing (and its side
    /// effects — the engine re-emits the title on every options change —
    /// are not nothing).
    pub fn configures_colors(&self) -> bool {
        false
    }

    /// The cell pixel size size reports answer with. This engine has none —
    /// its XTWINOPS replies are formatted by the consumer from its own
    /// metrics — so the call is a no-op, present because the facade surface
    /// is shared with the Ghostty backend (whose engine answers itself).
    pub fn set_cell_size(&mut self, _width: u32, _height: u32) {}

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(MinSize { cols, rows });
    }

    /// Read output. True when it brought a full-screen program's exit: the
    /// alternate screen was active when the call started and is not now.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let was_alt = self.alt();
        self.advance(bytes);
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

/// Whether the engine matcher can serve `query`, spelled the way the
/// backend's own matcher takes it — how a query that will not compile is
/// discovered before any terminal is touched, and (Phase 3) how one the
/// engine cannot serve at all reads as unavailable rather than silently
/// looser. Crate-internal: the dialect belongs to the search facade's
/// backend.
#[cfg_attr(feature = "ghostty", allow(dead_code))]
pub(crate) fn query_status(query: &SearchQuery) -> SearchStatus {
    if query.text.is_empty() {
        return SearchStatus::Idle;
    }
    if RegexSearch::new(&pattern_of(query)).is_err() {
        SearchStatus::InvalidPattern
    } else {
        SearchStatus::Pending
    }
}

/// The engine matcher's spelling of a query: literal text escaped unless
/// the query asks for a regex, and a case-sensitive anchor prefix when it
/// asks for that (the matcher defaults to case-insensitive).
fn pattern_of(query: &SearchQuery) -> String {
    let base = match query.regex {
        true => query.text.clone(),
        false => regex_escape(&query.text),
    };
    match query.case_sensitive {
        true => format!("(?-i){base}"),
        false => base,
    }
}

fn regex_escape(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for c in query.chars() {
        if matches!(
            c,
            '\\' | '.'
                | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '|'
                | '['
                | ']'
                | '{'
                | '}'
                | '^'
                | '$'
                | '#'
                | '&'
                | '-'
                | '~'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
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

// ---- The engine trait impls ------------------------------------------------

impl<T: EventListener> sealed::Sealed for Term<T> {}

impl<T: EventListener> SessionRead for Term<T> {
    fn columns_raw(&self) -> usize {
        self.columns()
    }
    fn screen_lines_raw(&self) -> usize {
        self.screen_lines()
    }
    fn history_size_raw(&self) -> usize {
        self.grid().history_size()
    }
    fn topmost_line_raw(&self) -> i32 {
        self.topmost_line().0
    }
    fn bottommost_line_raw(&self) -> i32 {
        self.bottommost_line().0
    }
    fn viewport_offset_raw(&self) -> usize {
        self.grid().display_offset()
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
    fn glyph_raw(&self, point: GridPoint) -> Glyph {
        glyph_of(&self.grid()[Line(point.line)][Column(usize::from(point.col))])
    }
    fn cell_raw(&self, point: GridPoint) -> Cell {
        cell_to_model(&self.grid()[Line(point.line)][Column(usize::from(point.col))])
    }
    fn row_wraps_raw(&self, line: i32) -> bool {
        let last = Column(self.columns() - 1);
        self.grid()[Line(line)][last].flags.contains(AlacFlags::WRAPLINE)
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
    fn has_selection_raw(&self) -> bool {
        self.selection.is_some()
    }
    fn selection_to_string_raw(&self) -> Option<String> {
        self.selection_to_string()
    }
    fn modes_raw(&self) -> Modes {
        Modes::from(self.mode())
    }
    fn cursor_cell_raw(&self) -> Option<(usize, usize)> {
        // The engine's own renderable view, read for the cursor alone: the
        // point it hands back is already adjusted off a wide character's
        // trailing spacer onto the lead cell, and display_offset moves it
        // into viewport coordinates. The row can run past the viewport's
        // bottom when that offset is scrolled up — the caller that anchors
        // the input bar there wants the row regardless.
        let content = self.renderable_content();
        let row = content.cursor.point.line.0 + content.display_offset as i32;
        let col = content.cursor.point.column.0;
        (row >= 0).then_some((row as usize, col))
    }
    fn semantic_escape_chars_raw(&self) -> &str {
        self.semantic_escape_chars()
    }
    fn search_all_raw(&self, query: &SearchQuery, cap: usize) -> (Vec<SearchMatch>, bool) {
        // The engine's own regex walker, driven exactly as the search UI
        // drove it directly before the facade existed: from the top of
        // scrollback rightward, one cell past each hit, until the cap, the
        // end of the grid, or a walk that wrapped back onto covered ground.
        let Ok(mut regex) = RegexSearch::new(&pattern_of(query)) else {
            return (Vec::new(), false);
        };
        let grid = self.grid();
        let mut origin = Point::new(grid.topmost_line(), Column(0));
        let mut matches = Vec::new();
        let mut last_start = None;
        while matches.len() < cap {
            let Some(m) =
                self.search_next(&mut regex, origin, Direction::Right, AlacSide::Left, None)
            else {
                break;
            };
            if last_start.is_some_and(|last| *m.start() <= last) {
                break;
            }
            last_start = Some(*m.start());
            origin = m.end().add(grid, Boundary::None, 1);
            let wrapped = origin <= *m.end();
            matches.push(SearchMatch {
                start: (*m.start()).into(),
                end: (*m.end()).into(),
            });
            if wrapped {
                break;
            }
        }
        let capped = matches.len() >= cap;
        (matches, capped)
    }

    fn frame_raw(&self, frame: &mut Frame, want_sliver: bool) {
        let cols = self.columns();
        let lines = self.screen_lines();
        // Size to the engine's grid before reading it, resizing in place so
        // a steady-state fill reuses every row and cell and allocates
        // nothing. A resize between fills only grows or shrinks the vecs.
        frame.rows.resize_with(lines, Row::default);
        for row in &mut frame.rows {
            row.cells.resize_with(cols, Cell::default);
        }
        frame.hyperlinks.clear();

        // The engine's own renderable view: display iteration, the resolved
        // selection, and a cursor already adjusted off a wide character's
        // trailing spacer onto the lead cell that owns the glyph — the same
        // facts the GUI's painter read off this engine directly, in the
        // same shapes.
        let content = self.renderable_content();
        let display_offset = content.display_offset;
        frame.display_offset = display_offset;
        frame.history_size = self.grid().history_size();
        frame.alt_screen = content.mode.contains(TermMode::ALT_SCREEN);
        // The same conversion `selection()` makes; the frame keeps the
        // kind because a block selection selects a rectangle inside its
        // span, not the span whole.
        frame.selection = self.selection_raw();

        for cell in content.display_iter {
            let row = cell.point.line.0 + display_offset as i32;
            let col = cell.point.column.0;
            // The engine cannot yield a point outside its own viewport;
            // the bounds check mirrors the guard the direct read had, so a
            // surprising engine never panics a frame into existence.
            if !((0..lines as i32).contains(&row) && col < cols) {
                continue;
            }
            let out = &mut frame.rows[row as usize];
            fill_frame_cell(&mut out.cells[col], cell.cell, &mut frame.hyperlinks);
            if col + 1 == cols {
                // The engine parks the wrap flag on the row's last cell.
                out.wrap = cell.cell.flags.contains(AlacFlags::WRAPLINE);
                // No row-level dirty tracking exists to read here; every
                // row reports dirty so a consumer never skips a rebuild.
                out.dirty = true;
            }
        }

        // The overscan row above the viewport: only scrollback has one.
        frame.sliver = None;
        if want_sliver && display_offset < frame.history_size {
            let line = Line(-(display_offset as i32) - 1);
            let grid_row = &self.grid()[line];
            let mut row = frame.sliver.take().unwrap_or_default();
            row.cells.resize_with(cols, Cell::default);
            for (col, out) in row.cells.iter_mut().enumerate() {
                fill_frame_cell(out, &grid_row[Column(col)], &mut frame.hyperlinks);
            }
            row.wrap = grid_row[Column(cols - 1)].flags.contains(AlacFlags::WRAPLINE);
            row.dirty = true;
            frame.sliver = Some(row);
        }

        // Viewport-relative, `None` once scrolled away — the renderer never
        // re-derives that arithmetic. `Hidden` (a DECTCEM reset) is a
        // visibility answer wearing a shape tag; the model keeps visibility
        // on the cursor proper.
        let cur = content.cursor;
        let row = cur.point.line.0 + display_offset as i32;
        frame.cursor =
            if (0..lines as i32).contains(&row) && cur.point.column.0 < cols {
                Some(FrameCursor {
                    x: cur.point.column.0 as u16,
                    y: row as u16,
                    shape: cur.shape.into(),
                    visible: !matches!(cur.shape, AlacCursorShape::Hidden),
                })
            } else {
                None
            };
    }
}

impl<T: EventListener> SessionTerm for Term<T> {
    fn cursor_style_raw(&self) -> CursorStyle {
        self.cursor_style().into()
    }
    fn set_cursor_style_raw(&mut self, style: Option<CursorStyle>) {
        self.set_cursor_style(style.map(Into::into));
    }
    fn clear_row_wrap_raw(&mut self, line: i32) {
        let last = Column(self.columns() - 1);
        self.grid_mut()[Line(line)][last].flags.remove(AlacFlags::WRAPLINE);
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
    fn clear_selection_raw(&mut self) {
        self.selection = None;
    }
    fn start_selection_raw(&mut self, kind: SelectionKind, point: GridPoint, side: Side) {
        self.selection =
            Some(AlacSelection::new(kind.into(), to_engine_point(point), side.into()));
    }
    fn update_selection_raw(&mut self, point: GridPoint, side: Side) {
        if let Some(selection) = self.selection.as_mut() {
            selection.update(to_engine_point(point), side.into());
        }
    }
    fn scroll_to_raw(&mut self, point: GridPoint) {
        self.scroll_to_point(to_engine_point(point));
    }
    fn scroll_raw(&mut self, scroll: Scroll) {
        let scroll = match scroll {
            Scroll::Delta(lines) => AlacScroll::Delta(lines),
            Scroll::PageUp => AlacScroll::PageUp,
            Scroll::PageDown => AlacScroll::PageDown,
            Scroll::Top => AlacScroll::Top,
            Scroll::Bottom => AlacScroll::Bottom,
        };
        self.scroll_display(scroll);
    }
    fn clear_scrollback_raw(&mut self) {
        // `CSI 3 J` as a direct engine op: purge the history, clamp the vi
        // cursor, and drop a selection anchored in the rows that went. A
        // backend with no such op can parse the sequence itself.
        self.clear_screen(ClearMode::Saved);
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

    fn track_point_raw(&mut self, _point: GridPoint) -> Option<crate::TrackedAnchor> {
        // This engine tracks nothing; its callers keep absolute rows.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The raw "before" arms of the pin tests below drive a bare engine
    // terminal; only they need the engine's null listener.
    use alacritty_terminal::event::VoidListener;

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
                    .map(|c| c.c)
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
                    .map(|c| c.c)
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
    /// the way `tty59 capture --plain` did before the facade. The pin tests
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

    /// The cursor's viewport cell: the display-offset arithmetic and the
    /// wide-spacer lead-cell adjustment a frame's cursor gets, without the
    /// frame's "inside the viewport" gate — a viewport scrolled into history
    /// still reports its cursor, in the row below the one a frame would
    /// paint, because the input bar anchors there. The scrolled-up arm is
    /// the Phase 3 pin: a backend whose render-state cursor goes silent
    /// outside the viewport must derive this row from the active-area cursor
    /// plus the viewport offset, not from that.
    #[test]
    fn the_cursor_cell_reads_viewport_relative_ungated() {
        let mut term = fed(10, 3, b"hi\r\nl2\r\nl3\r\nl4\r\nl5\r\n");
        assert_eq!(term.view().cursor_cell(), Some((2, 0)));
        term.session().scroll(Scroll::Delta(2));
        assert_eq!(
            term.view().cursor_cell(),
            Some((4, 0)),
            "below the viewport, still reported"
        );
        let mut frame = Frame::default();
        term.view().frame(&mut frame, false);
        assert!(frame.cursor.is_none(), "the frame gates what this read does not");

        // Parked on a wide character's spacer, the cell is the lead cell that
        // owns the glyph — the engine's own renderable-content adjustment.
        let mut term = fed(10, 3, "你 x".as_bytes());
        term.session().set_cursor(GridPoint { line: 0, col: 1 }, false);
        assert_eq!(term.view().cursor_cell(), Some((0, 0)));
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

    /// The gesture lifecycle the mouse paths drive: begin at one edge,
    /// update away from it, and — before the first update — a selection
    /// that is *set* but resolves to nothing. `has_selection` is the copy
    /// gate's earlier question; answering it with `selection().is_some()`
    /// would let the first click of a drag fall through as a signal.
    #[test]
    fn a_selection_starts_updates_and_gates_on_being_set() {
        let mut term = VtTerm::new(10, 3, 10);
        term.feed(b"one two\r\nthree");
        let mut session = term.session();
        session.start_selection(
            SelectionKind::Simple,
            GridPoint { line: 0, col: 4 },
            Side::Left,
        );
        assert!(session.has_selection(), "set from the anchor alone");
        assert_eq!(session.selection(), None, "but it covers no cells yet");
        assert_eq!(session.selection_to_string(), None);

        session.update_selection(GridPoint { line: 1, col: 4 }, Side::Right);
        assert!(session.has_selection());
        assert_eq!(
            session.selection_to_string().as_deref(),
            Some("two\nthree"),
            "the anchor held while the free end moved"
        );

        session.clear_selection();
        assert!(!session.has_selection());
        // An update with nothing active is a no-op, not a new selection.
        session.update_selection(GridPoint { line: 0, col: 0 }, Side::Left);
        assert!(!session.has_selection());
    }

    /// `Scroll`'s model targets land exactly where the engine's own scroll
    /// calls land — pinned target for target against a raw engine terminal
    /// fed the same bytes.
    #[test]
    fn the_viewport_scrolls_by_model_targets() {
        let bytes = b"l1\r\nl2\r\nl3\r\nl4\r\nl5\r\n";
        let mut term = fed(10, 3, bytes);
        assert_eq!(term.session().viewport_offset(), 0);

        let mut raw = Term::new(Config::default(), &MinSize { cols: 10, rows: 3 }, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut raw, bytes);
        let mut pin = |raw: &mut Term<VoidListener>, scroll: Scroll, engine: AlacScroll| {
            let mut session = term.session();
            session.scroll(scroll);
            raw.scroll_display(engine);
            assert_eq!(
                session.viewport_offset(),
                raw.grid().display_offset(),
                "{scroll:?} lands where the engine's own call does"
            );
        };
        pin(&mut raw, Scroll::Top, AlacScroll::Top);
        pin(&mut raw, Scroll::PageDown, AlacScroll::PageDown);
        pin(&mut raw, Scroll::PageUp, AlacScroll::PageUp);
        pin(&mut raw, Scroll::Delta(1), AlacScroll::Delta(1));
        pin(&mut raw, Scroll::Bottom, AlacScroll::Bottom);
        assert_eq!(term.session().viewport_offset(), 0);
    }

    /// Clearing the scrollback is `CSI 3 J` as an engine op: the history
    /// goes, a selection anchored in it goes with it, and one entirely on
    /// the live screen keeps.
    #[test]
    fn clearing_the_scrollback_drops_history_and_its_selections() {
        let mut term = fed(10, 3, b"l1\r\nl2\r\nl3\r\nl4\r\nl5\r\n");
        assert!(term.session().history_size() > 0, "the test needs history");
        term.session().start_selection(
            SelectionKind::Simple,
            GridPoint { line: -1, col: 0 },
            Side::Left,
        );
        term.session().clear_scrollback();
        assert_eq!(term.session().history_size(), 0);
        assert!(
            !term.session().has_selection(),
            "the selection pointed at rows that no longer exist"
        );
        assert_eq!(term.session().viewport_offset(), 0, "back on the live screen");
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
        // The union bit answers "is the program taking the mouse" for each
        // of the three reporting modes on its own, and for none of them off.
        term.feed(b"\x1b[?1000h");
        assert!(term.session().modes().intersects(Modes::MOUSE_MODE));
        term.feed(b"\x1b[?1000l\x1b[?1002h");
        assert!(term.session().modes().intersects(Modes::MOUSE_MODE));
        term.feed(b"\x1b[?1002l\x1b[?1003h");
        assert!(term.session().modes().intersects(Modes::MOUSE_MODE));
        term.feed(b"\x1b[?1003l");
        assert!(!term.session().modes().intersects(Modes::MOUSE_MODE));
        // The kitty keyboard protocol's flags: 1 disambiguates, 1+8 reports
        // every key, and 25 adds the text — the three a key encoder asks
        // for. The protocol is off in `Config::default` (the GUI's live term
        // turns it on), so the pin drives a raw engine terminal configured
        // the way the GUI configures its own.
        let mut raw = Term::new(
            Config { kitty_keyboard: true, ..Config::default() },
            &MinSize { cols: 10, rows: 3 },
            VoidListener,
        );
        let mut parser: Processor = Processor::new();
        let kitty =
            Modes::KITTY_DISAMBIGUATE | Modes::KITTY_REPORT_ALL_KEYS | Modes::KITTY_REPORT_TEXT;
        parser.advance(&mut raw, b"\x1b[>1u");
        assert_eq!(VtTermView::new(&raw).modes() & kitty, Modes::KITTY_DISAMBIGUATE);
        parser.advance(&mut raw, b"\x1b[>9u");
        assert_eq!(
            VtTermView::new(&raw).modes() & kitty,
            Modes::KITTY_DISAMBIGUATE | Modes::KITTY_REPORT_ALL_KEYS
        );
        parser.advance(&mut raw, b"\x1b[>25u");
        assert_eq!(VtTermView::new(&raw).modes() & kitty, kitty);
        // The embedder configures separators; the facade carries whatever
        // the terminal holds, here the engine default.
        assert_eq!(term.session().semantic_escape_chars(), ",│`|:\"' ()[]{}<>\t");
    }

    #[test]
    fn kitty_pushes_past_the_stack_cap_evict_the_bottom() {
        // The fork's patch #1: the keyboard-mode stack is capped at
        // `KEYBOARD_MODE_STACK_MAX_DEPTH` (4096) with the *bottom* dropped,
        // so a program that never pops cannot grow it without bound — the
        // 4097th push used to panic the reader. Bury one disambiguate push
        // under a cap's worth of event-type pushes, push once more (the
        // 4097th) and pop the lot back: the flags in effect end empty — the
        // bottom entry is gone, not merely buried. (Ghostty caps the same
        // stack at eight; its backend test pins that, and the daemon's
        // tracker mirrors whichever engine is compiled in.)
        let mut term = VtTerm::with_options(
            VtOptions {
                kitty_keyboard: true,
                scrollback_lines: 10,
                ..VtOptions::default()
            },
            10,
            3,
        );
        let mut bytes = b"\x1b[>1u".to_vec(); // the entry the cap will drop
        bytes.extend(b"\x1b[>2u".repeat(4095)); // fill the stack
        bytes.extend(b"\x1b[>8u"); // the 4097th push: drops the flags=1 bottom
        term.feed(&bytes);
        let kitty = Modes::KITTY_DISAMBIGUATE
            | Modes::KITTY_REPORT_EVENT_TYPES
            | Modes::KITTY_REPORT_ALL_KEYS;
        assert_eq!(term.session().modes() & kitty, Modes::KITTY_REPORT_ALL_KEYS);
        term.feed(b"\x1b[<1u");
        assert_eq!(term.session().modes() & kitty, Modes::KITTY_REPORT_EVENT_TYPES);
        term.feed(b"\x1b[<4095u");
        assert_eq!(
            term.session().modes() & kitty,
            Modes::empty(),
            "the cap dropped the 4097th-back push; an unbounded stack would still show it"
        );
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

    /// The view answers every read the session answers, over a shared
    /// borrow — the walkers and the composer's scans are the consumers this
    /// is for, and they read exactly these.
    #[test]
    fn a_read_only_view_answers_what_the_session_answers() {
        let mut term = fed(10, 3, b"one two\r\nthree");
        term.session().set_selection(
            SelectionKind::Simple,
            GridPoint { line: 0, col: 4 },
            Side::Left,
            GridPoint { line: 1, col: 4 },
            Side::Right,
        );
        let point = GridPoint { line: 0, col: 0 };
        let mid = GridPoint { line: 0, col: 4 };
        let end = GridPoint { line: 1, col: 4 };
        assert_eq!(term.view().columns(), term.session().columns());
        assert_eq!(term.view().screen_lines(), term.session().screen_lines());
        assert_eq!(term.view().topmost_line(), term.session().topmost_line());
        assert_eq!(term.view().history_size(), term.session().history_size());
        assert_eq!(term.view().bottommost_line(), term.session().bottommost_line());
        assert_eq!(term.view().glyph(point), term.session().glyph(point));
        assert_eq!(term.view().row_wraps(0), term.session().row_wraps(0));
        assert_eq!(term.view().text(mid, end), term.session().text(mid, end));
        assert_eq!(term.view().selection(), term.session().selection());
        assert_eq!(
            term.view().selection_to_string(),
            term.session().selection_to_string()
        );
        assert_eq!(term.view().modes(), term.session().modes());
        let seps = term.view().semantic_escape_chars().to_string();
        assert_eq!(seps, term.session().semantic_escape_chars());
    }

    /// The viewport offset reads how far a scroll into history carried the
    /// viewport off the live screen. The facade has no scroll of its own —
    /// the GUI drives the engine's — so the pin drives the raw engine the
    /// way the GUI does and reads the answer through the view.
    #[test]
    fn the_viewport_offset_counts_rows_scrolled_into_history() {
        let mut raw = Term::new(Config::default(), &MinSize { cols: 10, rows: 3 }, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut raw, b"l1\r\nl2\r\nl3\r\nl4\r\nl5\r\n");
        assert_eq!(VtTermView::new(&raw).viewport_offset(), 0);
        raw.scroll_display(AlacScroll::Delta(2));
        assert_eq!(VtTermView::new(&raw).viewport_offset(), 2);
    }

    /// The renderer's projection of a cell: colors kept unresolved the way
    /// [`StyleColor`] promises, the theme specials (an unstyled cell's
    /// foreground/background) already collapsed to "unset".
    #[test]
    fn a_cell_reads_style_the_renderer_blend_needs() {
        let term = fed(10, 3, b"\x1b[7;38;2;1;2;3;48;5;9mx\x1b[0m y");
        let cell = term.view().cell(GridPoint { line: 0, col: 0 });
        assert!(cell.inverse);
        assert_eq!(cell.fg, StyleColor::Rgb(model::Rgb { r: 1, g: 2, b: 3 }));
        assert_eq!(cell.bg, StyleColor::Palette(9));
        let plain = term.view().cell(GridPoint { line: 0, col: 2 });
        assert!(!plain.inverse);
        assert_eq!(plain.fg, StyleColor::None, "the theme default, not a color");
        assert_eq!(plain.bg, StyleColor::None);
    }

    /// A frame over the mixed-SGR screen the renderer actually meets:
    /// shape, unresolved colors, style flags, underline precedence.
    #[test]
    fn a_frame_reads_the_viewport_rows_and_their_style() {
        let term = fed(
            10,
            3,
            b"\x1b[1;3;4:3;38;2;1;2;3;48;5;9mx\x1b[0m y\x1b[2;9mz\x1b[0m",
        );
        let mut frame = Frame::default();
        term.view().frame(&mut frame, false);
        assert_eq!(frame.rows.len(), 3, "the engine's screen height");
        assert_eq!(frame.rows[0].cells.len(), 10, "the engine's width");
        let styled = &frame.rows[0].cells[0];
        assert_eq!(styled.c, 'x');
        assert!(styled.marks.is_empty());
        assert!(styled.bold && styled.italic);
        assert_eq!(styled.underline, Underline::Curly);
        assert_eq!(styled.fg, StyleColor::Rgb(model::Rgb { r: 1, g: 2, b: 3 }));
        assert_eq!(styled.bg, StyleColor::Palette(9));
        let faint = &frame.rows[0].cells[3];
        assert!(faint.dim && faint.strikethrough);
        // A cell no output reached reads as a plain space, the way the
        // engine being replaced stored it.
        assert_eq!(
            frame.rows[2].cells[0],
            Cell { c: ' ', ..Cell::default() }
        );
        assert_eq!(frame.cursor.map(|c| (c.x, c.y)), Some((4, 0)));
        assert_eq!(frame.display_offset, 0);
    }

    #[test]
    fn a_frame_reuses_its_rows_across_fills() {
        let mut term = fed(4, 2, b"abcd");
        let mut frame = Frame::default();
        term.view().frame(&mut frame, false);
        assert_eq!(frame.rows[0].cells[0].c, 'a');
        // New output lands on the next row; the same buffers carry it.
        term.feed(b"\r\nefgh");
        term.view().frame(&mut frame, false);
        assert_eq!(frame.rows[1].cells[0].c, 'e');
        // A scroll through the bottom shifts the viewport; the rows are
        // overwritten in place, not reallocated.
        term.feed(b"\r\ng");
        term.view().frame(&mut frame, false);
        assert_eq!(frame.rows[0].cells[0].c, 'e');
        assert_eq!(frame.rows[1].cells[0].c, 'g');
        // A resize re-shapes them.
        term.session().resize(6, 3);
        term.view().frame(&mut frame, false);
        assert_eq!(frame.rows.len(), 3);
        assert_eq!(frame.rows[0].cells.len(), 6);
    }

    #[test]
    fn a_frame_keeps_wide_pairs_and_their_spacers() {
        let term = fed(10, 3, "你好 x".as_bytes());
        let mut frame = Frame::default();
        term.view().frame(&mut frame, false);
        let row = &frame.rows[0].cells;
        assert_eq!(row[0].wide, CellWidth::Wide);
        assert_eq!(row[1].wide, CellWidth::SpacerTail);
        assert_eq!(row[2].wide, CellWidth::Wide);
        assert_eq!(row[5].wide, CellWidth::Narrow);
    }

    /// The selection travels as its resolved span plus the kind, which is
    /// what a block selection's rectangle needs to survive the crossing.
    #[test]
    fn a_frame_carries_the_selection_and_its_kind() {
        let mut term = fed(10, 3, b"one two\r\nthree");
        term.session().set_selection(
            SelectionKind::Simple,
            GridPoint { line: 0, col: 4 },
            Side::Left,
            GridPoint { line: 1, col: 4 },
            Side::Right,
        );
        let mut frame = Frame::default();
        term.view().frame(&mut frame, false);
        let sel = frame.selection.expect("the selection");
        assert_eq!(sel.kind, SelectionKind::Simple);
        assert_eq!(sel.span.start, GridPoint { line: 0, col: 4 });
        assert_eq!(sel.span.end, GridPoint { line: 1, col: 4 });
        assert!(sel.contains(GridPoint { line: 0, col: 4 }), "its own start");
        assert!(!sel.contains(GridPoint { line: 0, col: 0 }), "before its start");
        // The last row runs to the row's end before the span's end column.
        assert!(sel.contains(GridPoint { line: 1, col: 0 }));
        assert!(!sel.contains(GridPoint { line: 1, col: 5 }));
    }

    /// A block selection cuts every row at the same columns; `contains`
    /// must not select the span's middles end to end.
    #[test]
    fn a_block_selection_contains_only_its_rectangle() {
        let block = Selection {
            kind: SelectionKind::Block,
            span: SelectionSpan {
                start: GridPoint { line: 0, col: 2 },
                end: GridPoint { line: 2, col: 4 },
            },
        };
        assert!(block.contains(GridPoint { line: 1, col: 3 }));
        assert!(!block.contains(GridPoint { line: 1, col: 0 }), "left of the rectangle");
        assert!(!block.contains(GridPoint { line: 1, col: 9 }), "right of it");
        assert!(!block.contains(GridPoint { line: 0, col: 1 }), "before its left edge");
        // The case that makes the kinds differ: a simple selection's middle
        // rows run the full width, both past the rectangle's edges.
        let simple = Selection { kind: SelectionKind::Simple, ..block };
        assert!(simple.contains(GridPoint { line: 1, col: 0 }));
        assert!(simple.contains(GridPoint { line: 1, col: 9 }));
    }

    /// Cursor visibility: DECTCEM off reports a position but no caret;
    /// scrolled out of the viewport, no cursor at all.
    #[test]
    fn a_frame_reports_the_cursor_with_visibility_and_shape() {
        let mut term = fed(10, 3, b"hi");
        {
            let mut session = term.session();
            session.set_cursor_style(Some(CursorStyle {
                shape: CursorShape::Underline,
                blinking: false,
            }));
        }
        let mut frame = Frame::default();
        term.view().frame(&mut frame, false);
        let cur = frame.cursor.expect("the cursor");
        assert_eq!((cur.x, cur.y), (2, 0));
        assert_eq!(cur.shape, CursorShape::Underline);
        assert!(cur.visible);

        term.feed(b"\x1b[?25l");
        term.view().frame(&mut frame, false);
        let cur = frame.cursor.expect("the position survives");
        assert!(!cur.visible, "a DECTCEM reset hides the caret, not the point");

        // Scrolled a viewport up, the live cursor is below the frame.
        term.feed(b"\x1b[?25h\r\nl2\r\nl3\r\nl4\r\nl5\r\n");
        term.session().scroll(Scroll::Delta(2));
        term.view().frame(&mut frame, false);
        assert!(frame.cursor.is_none(), "the cursor scrolled out of the viewport");
        assert_eq!(frame.display_offset, 2);
        assert!(frame.history_size >= 2);
    }

    /// A cursor parked on a wide character's trailing spacer draws on the
    /// lead cell that owns the glyph — the engine's own
    /// `renderable_content` adjustment, carried through the frame.
    #[test]
    fn a_cursor_on_a_wide_tail_reports_the_lead_cell() {
        let mut term = fed(10, 3, "你 x".as_bytes());
        // Park it on the spacer half explicitly — the wrap flows that land
        // there naturally are the same state.
        term.session().set_cursor(GridPoint { line: 0, col: 1 }, false);
        let mut frame = Frame::default();
        term.view().frame(&mut frame, false);
        assert_eq!(frame.cursor.expect("the cursor").x, 0, "the lead cell");
    }

    /// The alt-screen flag rides the frame — the prompt-cursor restyle and
    /// the renderer both ask "is a full-screen program showing".
    #[test]
    fn a_frame_reports_the_alt_screen() {
        let mut term = fed(20, 4, b"before\r\n\x1b[?1049h\x1b[2J\x1b[1;1Halt");
        let mut frame = Frame::default();
        term.view().frame(&mut frame, false);
        assert!(frame.alt_screen);
        term.feed(b"\x1b[?1049l");
        term.view().frame(&mut frame, false);
        assert!(!frame.alt_screen);
    }

    /// The sliver: the row above the viewport whenever scrollback has one —
    /// including at the live screen, where a scroll animation's first frame
    /// reveals it — with the same cell fidelity the viewport rows get.
    #[test]
    fn a_frame_carries_the_sliver_row_when_scrolled() {
        let mut term = fed(10, 3, b"\x1b[31ml1\r\n\x1b[0ml2\r\nl3\r\nl4\r\nl5\r\n");
        term.session().scroll(Scroll::Delta(2));
        let mut frame = Frame::default();
        term.view().frame(&mut frame, true);
        let sliver = frame.sliver.as_ref().expect("a row above the viewport");
        assert_eq!(sliver.cells[0].c, 'l');
        assert_eq!(sliver.cells[0].fg, StyleColor::Palette(1), "SGR crosses with it");
        assert_eq!(frame.rows[0].cells[0].c, 'l', "the viewport below it");

        // Back at the live screen the row above it is still scrollback's
        // last row: the first frame of a scroll-up reveals exactly it.
        term.session().scroll(Scroll::Delta(-2));
        term.view().frame(&mut frame, true);
        let sliver = frame.sliver.as_ref().expect("the row above the live screen");
        assert_eq!(sliver.cells[1].c, '3');

        // No scrollback at all: nothing above to give, even when asked.
        let bare = fed(10, 2, b"solo");
        let mut frame = Frame::default();
        bare.view().frame(&mut frame, true);
        assert!(frame.sliver.is_none());
    }

    /// Stacked underline SGRs resolve by the engine's own precedence — the
    /// order the flags were checked in before the model carried one enum.
    #[test]
    fn underline_flags_resolve_in_the_engines_precedence() {
        assert_eq!(underline_of(AlacFlags::empty()), Underline::None);
        assert_eq!(underline_of(AlacFlags::UNDERLINE), Underline::Single);
        assert_eq!(
            underline_of(AlacFlags::UNDERLINE | AlacFlags::UNDERCURL),
            Underline::Curly
        );
        assert_eq!(
            underline_of(AlacFlags::DOUBLE_UNDERLINE | AlacFlags::DASHED_UNDERLINE),
            Underline::Double
        );
    }

    /// OSC 8 anchors cross as side-table indexes: one index per distinct
    /// URI, so a run of one link shares it and the table maps back.
    #[test]
    fn a_frame_interns_hyperlink_uris() {
        let term = fed(
            30,
            3,
            "a \x1b]8;;https://e.com\x1b\\run\x1b]8;;\x1b\\ b \x1b]8;;https://f.io\x1b\\two\x1b]8;;\x1b\\".as_bytes(),
        );
        let mut frame = Frame::default();
        term.view().frame(&mut frame, false);
        let row = &frame.rows[0].cells;
        let run = row[2].hyperlink.expect("the linked run");
        assert_eq!(row[3].hyperlink, Some(run), "one link, one index");
        assert!(row[0].hyperlink.is_none());
        assert_eq!(frame.hyperlinks[run as usize], "https://e.com");
        let two = row[8].hyperlink.expect("the second link");
        assert_ne!(two, run);
        assert_eq!(frame.hyperlinks[two as usize], "https://f.io");
    }

    // ---- The listener path -------------------------------------------------

    /// An event collector the `&self`-shaped listener protocol can hand
    /// events to — the same shape the GUI's proxy has (a channel behind a
    /// shared borrow).
    #[derive(Default, Clone)]
    struct Tap(std::sync::Arc<std::sync::Mutex<Vec<VtEvent>>>);

    impl VtEventListener for Tap {
        fn send_event(&self, event: VtEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn tapped() -> (VtTerm<Tap>, Tap) {
        let tap = Tap::default();
        let term = VtTerm::with_listener(VtOptions::default(), 20, 4, tap.clone());
        (term, tap)
    }

    /// Engine effects cross into model events, in order, through whatever
    /// listener the terminal was built with — and the engine's hints no
    /// consumer ever matched (mouse-cursor dirty, cursor-blink) vanish at
    /// the boundary rather than arriving as engine-typed events.
    #[test]
    fn engine_events_arrive_as_model_events_in_order() {
        let (mut term, tap) = tapped();
        term.advance(b"\x1b]2;the title\x07\x07\x1b[6n");
        term.exit();
        let events = tap.0.lock().unwrap().clone();
        assert_eq!(events[0], VtEvent::Title("the title".into()));
        assert_eq!(events[1], VtEvent::Bell);
        // A DSR cursor-position report: the engine answers queries by
        // asking the PTY to be written.
        assert!(matches!(&events[2], VtEvent::PtyWrite(bytes) if !bytes.is_empty()));
        assert_eq!(events.last(), Some(&VtEvent::Exit), "{events:?}");
    }

    /// The reply formatters a query arms take model types: `OSC 11 ?` hands
    /// the theme color back as a model [`model::Rgb`], formatted the way
    /// the engine's own reply wants it.
    #[test]
    fn a_color_request_formats_a_model_rgb() {
        let (mut term, tap) = tapped();
        term.advance(b"\x1b]11;?\x07");
        let events = tap.0.lock().unwrap().clone();
        let [VtEvent::ColorRequest(_, fmt)] = events.as_slice() else {
            panic!("one color request, not {events:?}");
        };
        let reply = fmt(model::Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(reply, "\x1b]11;rgb:0101/0202/0303\x07", "the engine's own OSC 11 reply shape");
    }

    /// Options map field for field onto the engine's own configuration —
    /// `VtOptions::default()` is the engine's `Config::default()`, and the
    /// two flags the GUI sets apart from it (kitty keyboard, conpty resize)
    /// survive the crossing.
    #[test]
    fn options_map_onto_the_engine_config() {
        let config = config_of(&VtOptions::default());
        let raw = Config::default();
        assert_eq!(config.scrolling_history, raw.scrolling_history);
        assert_eq!(config.semantic_escape_chars, raw.semantic_escape_chars);
        assert_eq!(config.default_cursor_style, raw.default_cursor_style);
        assert!(!config.kitty_keyboard && !config.conpty_resize);

        let gui = config_of(&VtOptions {
            kitty_keyboard: true,
            conpty_resize: true,
            cursor_style: CursorStyle {
                shape: CursorShape::Underline,
                blinking: false,
            },
            ..VtOptions::default()
        });
        assert!(gui.kitty_keyboard && gui.conpty_resize);
        assert_eq!(gui.default_cursor_style.shape, AlacCursorShape::Underline);
    }

    /// The reader's synchronized-update handling, through the facade: a BSU
    /// sets a deadline, `stop_sync` flushes and clears it.
    #[test]
    fn a_held_sync_update_reports_its_deadline_until_flushed() {
        let (mut term, _tap) = tapped();
        assert_eq!(term.sync_timeout(), None);
        term.advance(b"\x1b[?2026habc");
        assert!(term.sync_timeout().is_some(), "the BSU opened a window");
        term.stop_sync();
        assert_eq!(term.sync_timeout(), None);
    }
}

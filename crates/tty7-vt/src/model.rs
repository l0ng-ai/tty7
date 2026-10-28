//! The engine-neutral VT model: tty7's own vocabulary for a terminal's
//! state, shared by the GUI, the gateway and the CLI
//! (docs/libghostty-migration.md, Phase 2).
//!
//! Every type here is plain data — no FFI, no engine crate, no C archive —
//! so the whole seam compiles and is tested in builds that never link
//! libghostty-vt (CI's view). Backends live elsewhere in this crate and
//! convert *to and from* these types at their boundary; a consumer that
//! names an engine type is a consumer the migration has not reached yet.
//!
//! The shape follows the engine being replaced where its semantics were
//! right (`line` counts scrollback backwards from the screen, an unwritten
//! cell reads as a space) so each backend converts mechanically and the
//! differential tests can diff snapshots directly.

use std::fmt;

/// An RGB color, 8 bits per channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// A style color: what the program actually asked for, kept unresolved — a
/// palette index stays an index until the embedder maps it against its
/// theme at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum StyleColor {
    /// Unset — the theme's default for the role (foreground or background,
    /// depending on which field carries it).
    #[default]
    None,
    /// A 256-color palette index (SGR 30–37/90–97/38;5;N and backgrounds).
    Palette(u8),
    /// A direct color (SGR 38;2;R;G;B and backgrounds).
    Rgb(Rgb),
}

/// A cell's width class. Renderers draw `Wide` across two cells and skip
/// the two spacer kinds; text extraction joins wide pairs and drops them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum CellWidth {
    /// One column.
    #[default]
    Narrow,
    /// Two columns — this cell holds the text, the one after it is a
    /// `SpacerTail`.
    Wide,
    /// The continuation cell after a wide character. No text, no rendering.
    SpacerTail,
    /// The last cell of a soft-wrapped row holding back a wide character
    /// that moved to the next row. No text, no rendering.
    SpacerHead,
}

/// What OSC 133 marked a cell as part of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Semantic {
    /// Command output (the default for anything unmarked).
    #[default]
    Output,
    /// User input at the prompt.
    Input,
    /// The shell prompt itself.
    Prompt,
}

/// SGR underline style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// The DECSCUSR cursor shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum CursorShape {
    /// Filled rectangle (`CSI 2 q`).
    #[default]
    Block,
    /// Vertical bar (`CSI 5 q`).
    Beam,
    /// Underscore (`CSI 3 q`).
    Underline,
    /// The unfocused block a renderer draws (both engines carry it; DECSCUSR
    /// itself has no code for it).
    HollowBlock,
}

/// A cursor shape plus whether it blinks — what DECSCUSR sets and a
/// reset restores, and what [`crate::model::CursorShape`] alone can't say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct CursorStyle {
    pub shape: CursorShape,
    pub blinking: bool,
}

/// What an embedder configures the engine with, in the model's own
/// vocabulary — what `terminal_config_from_user` used to build as the
/// replaced engine's own `Config`. A backend maps this onto whatever its
/// engine's options are; a field a backend cannot honor is a gap the
/// differential tests name, not a silent behavior change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VtOptions {
    /// How many lines of scrollback the grid keeps.
    pub scrollback_lines: usize,
    /// The cursor a DECSCUSR reset (`0 q`) restores — what the configured
    /// `cursor_style` means to a consumer that restyles cursors around
    /// shell marks.
    pub cursor_style: CursorStyle,
    /// The characters that end a word for selection — the embedder's
    /// configuration, not a constant.
    pub semantic_escape_chars: String,
    /// Speak the kitty keyboard protocol when a program asks (the flag
    /// stack and the query replies it implies).
    pub kitty_keyboard: bool,
    /// Resize the way conhost re-anchors after a ConPTY resize (fork patch
    /// #3): grow pins rows and opens blanks below, shrink pulls the last
    /// written row to the new bottom.
    pub conpty_resize: bool,
}

impl Default for VtOptions {
    /// The engine being replaced's own defaults, so a backend mapping
    /// `VtOptions::default()` lands where its engine's `Default` does.
    fn default() -> Self {
        Self {
            scrollback_lines: 10_000,
            cursor_style: CursorStyle::default(),
            semantic_escape_chars: ",│`|:\"' ()[]{}<>\t".to_owned(),
            kitty_keyboard: false,
            conpty_resize: false,
        }
    }
}

/// A terminal's size as a query reply reports it: the grid plus the cell
/// pixel size a `CSI 14 t` answer carries (`CSI 4 ; height ; width t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct WindowSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

/// A point in the grid. `line` is 0 at the top of the *active area* (the
/// screen the cursor can move over) and counts backwards through
/// scrollback as it goes negative — the coordinate the engine being
/// replaced indexed by, and the one selections and marks have to keep
/// meaning across a reflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct GridPoint {
    pub line: i32,
    pub col: u16,
}

/// Which edge of a cell a selection boundary sits on — the difference
/// between the last column of a wrapped line and the first of the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Side {
    #[default]
    Left,
    Right,
}

/// A rectangular span of the grid, inclusive on both ends, in
/// [`GridPoint`] coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct SelectionSpan {
    pub start: GridPoint,
    pub end: GridPoint,
}

/// How a selection was made — the gesture that started it, which is what
/// decides how its span converts back to text (a `Lines` selection keeps
/// whole rows and ends in a newline; a `Block` cuts every row at the same
/// columns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectionKind {
    /// Dragged cell by cell.
    Simple,
    /// Word-wise, expanded to the separators on both ends.
    Semantic,
    /// Whole logical lines.
    Lines,
    /// A rectangle.
    Block,
}

/// The active selection: how it was made, plus the span of grid it covers,
/// normalized so `start` is the top-left end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Selection {
    pub kind: SelectionKind,
    pub span: SelectionSpan,
}

/// Where a scroll moves the viewport. Scrolling is view-only: it changes
/// what the viewport shows, never the active area or the PTY. The row
/// convention matches [`GridPoint`]'s screen — positive rows go up, into
/// scrollback — so a drag that runs off the top edge and a wheel notch are
/// the same `Delta` with the same sign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scroll {
    /// Rows relative to the viewport; positive goes up into scrollback,
    /// negative comes back down toward the live screen.
    Delta(i32),
    /// Up one viewport height.
    PageUp,
    /// Down one viewport height.
    PageDown,
    /// The top of the scrollback.
    Top,
    /// The bottom — the live, active area.
    Bottom,
}

impl Selection {
    /// Whether the cell at `point` is inside the selection. The span is the
    /// selection's resolved bounding box; `kind` is what makes a `Block`
    /// selection a rectangle inside it — for every other kind a row between
    /// `start.line` and `end.line` is selected end to end, which is what a
    /// drag across soft-wrapped rows means.
    pub fn contains(&self, point: GridPoint) -> bool {
        let SelectionSpan { start, end } = self.span;
        start.line <= point.line
            && end.line >= point.line
            && (start.col <= point.col
                || (start.line != point.line && self.kind != SelectionKind::Block))
            && (end.col >= point.col
                || (end.line != point.line && self.kind != SelectionKind::Block))
    }
}

/// One search match: the span of grid it covers, inclusive, in the same
/// coordinates a [`SelectionSpan`] uses. Ordered with the grid (line, then
/// column), which is the order a scan meets them in — so "the next match"
/// is "the next one in this order".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SearchMatch {
    pub start: GridPoint,
    pub end: GridPoint,
}

/// What a search looks for, and how loosely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    /// Read `text` as a regular expression rather than literal text.
    pub regex: bool,
    /// Distinguish ASCII letter case. Off by default, the way a find bar
    /// defaults — and the way a plain-text engine matches natively.
    pub case_sensitive: bool,
}

/// Which way a step through the matches moves. Named by content age, not
/// list position, because the two engines order their lists opposite ways:
/// this says "toward the match the next newer row holds" whichever is which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchDirection {
    /// Toward the live end of the grid — the next match at or below the
    /// current one, wrapping at the newest.
    Newer,
    /// Toward the top of the scrollback — the next match at or above the
    /// current one, wrapping at the oldest.
    Older,
}

/// Where a search stands. A scan over a large scrollback is real work, so
/// an engine may split it across steps; these are the states its caller
/// can be told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchStatus {
    /// No query is set (or its text is empty): nothing to look for.
    Idle,
    /// The query is a pattern that does not compile.
    InvalidPattern,
    /// A query is set but has not run against the terminal since it
    /// changed — a feed is owed.
    Pending,
    /// Caught up with the terminal as of the last feed. Never "finished
    /// forever": terminal writes after the last feed are unseen until the
    /// next one.
    Complete,
}

/// Whether selecting a match moves the viewport to show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchScroll {
    /// Scroll only if the match is not already readable. `hidden` is how
    /// many rows at the top of the viewport the embedder's own chrome
    /// covers (a floating find bar) — a match under it is on screen and
    /// still unreadable, which is the one thing "next match" must never do.
    IfNeeded {
        hidden: usize,
    },
    /// Leave the viewport where it is.
    None,
}

// The diffable projection of a `Cell`'s styling: one bitmask instead of a
// dozen fields, for the compare-and-skip a renderer does per cell. Rebuilt
// from the cell, never parsed. (A `//` comment because the macro swallows
// doc comments on the invocation itself.)
bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
    pub struct CellFlags: u16 {
        const BOLD = 1 << 0;
        const DIM = 1 << 1;
        const ITALIC = 1 << 2;
        const BLINK = 1 << 3;
        const INVERSE = 1 << 5;
        const INVISIBLE = 1 << 6;
        const STRIKEOUT = 1 << 7;
        const OVERLINE = 1 << 8;
        const UNDERLINED = 1 << 9;
        const WIDE = 1 << 10;
    }
}

impl From<&Cell> for CellFlags {
    fn from(cell: &Cell) -> Self {
        let mut flags = Self::empty();
        flags.set(Self::BOLD, cell.bold);
        flags.set(Self::DIM, cell.dim);
        flags.set(Self::ITALIC, cell.italic);
        flags.set(Self::BLINK, cell.blink);
        flags.set(Self::INVERSE, cell.inverse);
        flags.set(Self::INVISIBLE, cell.invisible);
        flags.set(Self::STRIKEOUT, cell.strikethrough);
        flags.set(Self::OVERLINE, cell.overline);
        flags.set(Self::UNDERLINED, cell.underline != Underline::None);
        flags.set(Self::WIDE, cell.wide == CellWidth::Wide);
        flags
    }
}

// The terminal modes a consumer can ask about. A bitmask because the
// questions come as "which of these changed since last frame"; grows one
// flag at a time as consumers migrate — not an attempt to enumerate the
// mode space.
bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
    pub struct Modes: u32 {
        /// Auto-wrap (DECAWM, `CSI ? 7`).
        const WRAPAROUND = 1 << 0;
        /// Application cursor keys (`CSI ? 1`).
        const APP_CURSOR = 1 << 1;
        /// Application keypad (`ESC =`).
        const APP_KEYPAD = 1 << 2;
        /// Cursor visible (DECTCEM, `CSI ? 25`).
        const SHOW_CURSOR = 1 << 3;
        /// Alternate screen (`CSI ? 1047`/`1049`).
        const ALT_SCREEN = 1 << 4;
        /// Bracketed paste (`CSI ? 2004`).
        const BRACKETED_PASTE = 1 << 5;
        /// Focus in/out reports (`CSI ? 1004`).
        const FOCUS_IN_OUT = 1 << 6;
        /// Click mouse reporting (`CSI ? 1000`).
        const MOUSE_REPORT_CLICK = 1 << 7;
        /// Drag mouse reporting (`CSI ? 1002`).
        const MOUSE_DRAG = 1 << 8;
        /// Motion mouse reporting (`CSI ? 1003`).
        const MOUSE_MOTION = 1 << 9;
        /// SGR mouse encoding (`CSI ? 1006`).
        const SGR_MOUSE = 1 << 10;
        /// Origin mode (DECOM, `CSI ? 6`).
        const ORIGIN = 1 << 11;
        /// Kitty keyboard protocol, flag 1 (`CSI > 1 u`): escape codes
        /// disambiguated, so every key has an encoding of its own.
        const KITTY_DISAMBIGUATE = 1 << 12;
        /// Kitty keyboard protocol, flag 8 (`CSI > 8 u`): report all keys as
        /// escape codes, not just the ones legacy encodings can spell.
        const KITTY_REPORT_ALL_KEYS = 1 << 13;
        /// Kitty keyboard protocol, flag 16 (`CSI > 16 u`): report the text
        /// associated with a key alongside its code.
        const KITTY_REPORT_TEXT = 1 << 14;
        /// UTF-8 mouse encoding (`CSI ? 1005`) — a way to *encode* reports,
        /// not a way to request them, which is why it sits outside the three
        /// reporting modes above.
        const UTF8_MOUSE = 1 << 15;
        /// Kitty keyboard protocol, flag 2 (`CSI > 2 u`): report event types
        /// (press/release). No encoder of ours consumes it yet; it rides the
        /// model so a mode report can carry the protocol's whole state.
        const KITTY_REPORT_EVENT_TYPES = 1 << 16;
        /// Kitty keyboard protocol, flag 4 (`CSI > 4 u`): report alternate
        /// keys. As [`Modes::KITTY_REPORT_EVENT_TYPES`].
        const KITTY_REPORT_ALTERNATE_KEYS = 1 << 17;
        /// Alternate scroll (`CSI ? 1007`): on the alternate screen the
        /// wheel moves the cursor (as arrow keys) instead of a scrollback
        /// that isn't there — the fact a wheel router has to ask for.
        const ALTERNATE_SCROLL = 1 << 18;
        /// Any mouse reporting at all — click, drag or motion — as one bit,
        /// the way the engine being replaced spelled the same union
        /// (`TermMode::MOUSE_MODE`): "is the program taking the mouse" is
        /// the question a wheel router and a hover gate ask, and they do not
        /// care which of the three brought it.
        const MOUSE_MODE =
            Self::MOUSE_REPORT_CLICK.bits() | Self::MOUSE_DRAG.bits() | Self::MOUSE_MOTION.bits();
    }
}

/// Which screen buffer is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Screen {
    #[default]
    Primary,
    Alternate,
}

/// The cursor: position within the active area plus the facets a renderer
/// or a redraw needs. `x`/`y` are 0-indexed, `y` relative to the top of
/// the active area (not the viewport, not the scrollback).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    /// DEC mode 25 (DECTCEM).
    pub visible: bool,
    /// The next printable cell soft-wraps to the following row.
    pub pending_wrap: bool,
    /// Which screen the cursor is on.
    pub screen: Screen,
}

/// The text and style of one grid cell. An unwritten cell reads as a
/// single space, the way the engine being replaced stored it — blank-line
/// detection (`c == ' '` with no marks and a default background) depends
/// on it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
pub struct Cell {
    /// The grapheme cluster's base codepoint — the glyph the grid cell was
    /// budgeted for. `'\0'` marks a cell with no text at all (the spacer
    /// half of a wide pair, on the engines that report one as empty).
    pub c: char,
    /// The rest of the cluster (combining marks, variation selectors):
    /// inline for what a terminal actually holds, the heap only past that.
    /// A base codepoint plus extras is the same shape the engines report
    /// graphemes in, so a backend copies straight into it — no string per
    /// cell on the frame path, where this is written per cell per paint.
    pub marks: smallvec::SmallVec<[char; 2]>,
    pub wide: CellWidth,
    pub fg: StyleColor,
    pub bg: StyleColor,
    pub underline_color: StyleColor,
    pub bold: bool,
    pub italic: bool,
    /// SGR 2. (One field for one escape, on both engines — Ghostty calls
    /// it "faint", the engine being replaced "dim".)
    pub dim: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: Underline,
    /// Index into [`Snapshot::hyperlinks`]. The engines expose a URI per
    /// cell, not a numeric id; the side table keeps hover and underline
    /// logic (same URI ⇒ same link) working without a string per cell.
    pub hyperlink: Option<u16>,
    pub semantic: Semantic,
}

impl Cell {
    /// The whole cluster as one string — the text-extraction view of a
    /// cell. Allocated per call; the render path reads `c`/`marks`
    /// directly.
    pub fn text(&self) -> String {
        std::iter::once(self.c).chain(self.marks.iter().copied()).collect()
    }

    /// The blank test a screen redraw uses: a cell nothing but the theme
    /// shows. Blank *cells* at a line's end are left unwritten; a cell a
    /// program painted — even a plain space with a color — is content.
    pub fn is_blank(&self) -> bool {
        self.c == ' '
            && self.marks.is_empty()
            && self.bg == StyleColor::None
            && !self.inverse
            && self.underline == Underline::None
            && !self.strikethrough
    }
}

/// What grid-walking logic reads off one cell: the primary character (the
/// base of its cluster, without the combining marks — a walker matches on
/// what was typed, not on its decorations), the width class that says
/// whether the cell is half of a wide pair, and the OSC 8 anchor under it,
/// if any.
///
/// [`Cell`] is the renderer's projection of a cell; this is the letters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Glyph {
    pub c: char,
    pub wide: CellWidth,
    /// The URI of the OSC 8 hyperlink this cell anchors, if it anchors one.
    /// Both engines identify a link by URI per cell (no numeric id) — the
    /// same rule a run of identically-linked cells walks by.
    pub hyperlink: Option<String>,
}

/// One grid row.
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// The row soft-wrapped into the next one — no line break after it.
    pub wrap: bool,
    /// The row is the continuation of a soft-wrapped row above.
    pub wrap_continuation: bool,
    /// The engine marked the row dirty since the last render pull.
    pub dirty: bool,
}

impl Row {
    /// One past the last cell with content: trailing padding is not
    /// spaces. A wrapped row counts as fully occupied.
    pub fn occupied(&self) -> usize {
        if self.wrap {
            return self.cells.len();
        }
        self.cells
            .iter()
            .rposition(|c| !c.is_blank())
            .map_or(0, |i| i + 1)
    }
}

/// An owned copy of a grid — scrollback plus screen — safe to use with no
/// lock held: every value below is plain Rust, and nothing in it
/// references a live engine.
///
/// `rows` holds `history_rows` rows of scrollback first, then the
/// `screen_rows` rows of the active area, top to bottom.
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
pub struct Snapshot {
    pub cols: u16,
    /// The active area's height in rows.
    pub screen_rows: u16,
    /// How many of `rows` are scrollback above the active area.
    pub history_rows: usize,
    pub cursor: Cursor,
    pub modes: Modes,
    /// The URI side table [`Cell::hyperlink`] indexes into.
    pub hyperlinks: Vec<String>,
    /// Scrollback first, then screen, top to bottom.
    pub rows: Vec<Row>,
}

impl Snapshot {
    /// The screen rows alone (what a viewport pinned to the bottom shows).
    pub fn screen(&self) -> &[Row] {
        &self.rows[self.history_rows..]
    }

    /// The scrollback rows alone.
    pub fn history(&self) -> &[Row] {
        &self.rows[..self.history_rows]
    }
}

/// The cursor a frame paints, already placed in the viewport: `y` is 0 at
/// the top visible row, so a renderer never re-derives scroll arithmetic.
/// The position is where the caret *draws* — a cursor parked on the second
/// half of a wide character is reported on the lead cell that owns the
/// glyph.
///
/// `visible: false` is a DECTCEM reset (`CSI ? 25 l`): a TUI that hides the
/// hardware cursor is usually drawing a caret of its own that ours must not
/// cover, so the renderer keeps the position (the IME wants it) and drops
/// the caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct FrameCursor {
    pub x: u16,
    pub y: u16,
    pub shape: CursorShape,
    pub visible: bool,
}

/// The viewport a renderer paints, as owned data: rows of cells plus the
/// scroll, cursor, selection and link state the painter needs. A backend
/// fills it while it holds its engine lock (the Ghostty rule: borrowed rows
/// and cells die at the next mutation), and the renderer reads it with no
/// lock held at all.
///
/// Filling reuses the frame: rows and cells are overwritten in place, so a
/// steady-state stream of frames allocates nothing. Shaped to be fillable
/// from Ghostty's render-state API (`render.h`): rows arrive top-to-bottom
/// with per-row dirty flags, the cursor reports viewport-relative with a
/// visibility bit, and the row above the viewport that smooth scrolling
/// half-shows is just an overscan row — [`Frame::sliver`] here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frame {
    /// The viewport's rows, top to bottom. `rows.len()` is the engine's
    /// screen height and `rows[r].cells.len()` its width at fill time; a
    /// renderer painting a different shape skips what falls outside its
    /// own bounds and paints nothing where the frame has no row.
    pub rows: Vec<Row>,
    /// The URI side table [`Cell::hyperlink`] indexes into, rebuilt each
    /// fill.
    pub hyperlinks: Vec<String>,
    /// How far the viewport is scrolled up from the live screen: 0 means
    /// the live screen itself is showing.
    pub display_offset: usize,
    /// Scrollback rows above the live screen.
    pub history_size: usize,
    /// The cursor, when it is inside the viewport; `None` when it has
    /// scrolled out of it (or onto the alt screen while the primary is
    /// showing, whichever way the engine counts).
    pub cursor: Option<FrameCursor>,
    /// The alternate screen is showing.
    pub alt_screen: bool,
    /// The active selection, if any, in absolute grid coordinates (the
    /// same space [`GridPoint::line`] addresses, scrollback negative).
    pub selection: Option<Selection>,
    /// The row just above the viewport, when the caller asked for one and
    /// scrollback has one to give — what a smooth scroll half-shows at the
    /// top edge.
    pub sliver: Option<Row>,
}

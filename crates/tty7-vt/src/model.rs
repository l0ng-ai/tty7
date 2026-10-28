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
/// detection (`text == " "` with a default background) depends on it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
pub struct Cell {
    /// The full grapheme cluster (base plus combining marks). Empty only
    /// for a wide character's spacer cell.
    pub text: String,
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
    /// The blank test a screen redraw uses: a cell nothing but the theme
    /// shows. Blank *cells* at a line's end are left unwritten; a cell a
    /// program painted — even a plain space with a color — is content.
    pub fn is_blank(&self) -> bool {
        self.text == " "
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

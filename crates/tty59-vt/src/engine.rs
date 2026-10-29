//! The backend contract: the engine-neutral traits the backend implements,
//! and the borrowing facades consumers reach it through.
//!
//! [`SessionRead`] is every read the seam's consumers do, [`SessionTerm`] the
//! writes a session adds, and both are *sealed* — implemented in this crate
//! (the libghostty-vt backend since Phase 6b; its replaced predecessor
//! behind a feature before that) and nowhere else, so the traits can keep
//! evolving without becoming public API. [`VtTermSession`] and
//! [`VtTermView`] sit over `dyn SessionTerm`/`dyn SessionRead` exactly as
//! they did when the traits lived in the replaced engine's backend; through
//! the migration they let the differential harness hold one view over each
//! engine at once.

use crate::model::{
    Cell, Cursor, Frame, Glyph, GridPoint, Modes, Scroll, SearchMatch, SearchQuery, Selection,
    SelectionKind, Side,
};

// Sealing: the supertrait is in a private module, so no type outside this
// crate can implement `SessionRead`. The traits are `pub` only because the
// facade constructors name them in bounds — sealed, they are contract, not
// extension point.
pub(crate) mod sealed {
    pub trait Sealed {}
}

/// The read-only engine surface, shared by [`VtTermSession`] (which adds the
/// writes) and [`VtTermView`]. Splitting here is what makes the view honest:
/// it holds `&dyn SessionRead`, so a consumer that only reads cannot reach a
/// mutating method even by mistake.
pub trait SessionRead: sealed::Sealed {
    fn columns_raw(&self) -> usize;
    fn screen_lines_raw(&self) -> usize;
    fn history_size_raw(&self) -> usize;
    fn topmost_line_raw(&self) -> i32;
    fn bottommost_line_raw(&self) -> i32;
    /// How far the viewport is scrolled up from the live screen, in rows.
    fn viewport_offset_raw(&self) -> usize;
    fn cursor_raw(&self) -> Cursor;
    fn glyph_raw(&self, point: GridPoint) -> Glyph;
    fn cell_raw(&self, point: GridPoint) -> Cell;
    fn row_wraps_raw(&self, line: i32) -> bool;
    fn text_raw(&self, from: GridPoint, to: GridPoint) -> String;
    fn selection_raw(&self) -> Option<Selection>;
    /// Whether a selection is *set*, before it is resolved against the grid
    /// — a drag's first click sets one that resolves to no span yet, and a
    /// copy gate has to see it (see `has_selection` below).
    fn has_selection_raw(&self) -> bool;
    fn selection_to_string_raw(&self) -> Option<String>;
    fn modes_raw(&self) -> Modes;
    fn semantic_escape_chars_raw(&self) -> &str;
    /// Every match of `query` in the whole grid, oldest first, capped at
    /// `cap` — and whether the cap stopped it. Search-shaped, and private to
    /// the crate: the query's dialect (regex, case) is the engine's own to
    /// accept or refuse — the caller learns which through the search
    /// facade's status, not by probing here.
    fn search_all_raw(&self, query: &SearchQuery, cap: usize) -> (Vec<SearchMatch>, bool);
    /// The cursor's cell in viewport coordinates — the same lead-cell
    /// adjustment a frame's cursor gets, without the "inside the viewport"
    /// gate: a viewport scrolled into history still reports its cursor, in
    /// the row below the one a frame would paint — a Phase 3 contract (see
    /// [`VtTermView::cursor_cell`] for why a render-state cursor alone
    /// cannot back it). `None` only when a backend cannot place the cursor
    /// on the screen at all.
    fn cursor_cell_raw(&self) -> Option<(usize, usize)>;
    /// Fill the frame a renderer paints, reusing its rows. The whole point
    /// of the frame: everything a paint needs, copied out under the lock a
    /// reader already holds, ownable with the lock dropped.
    fn frame_raw(&self, frame: &mut Frame, want_sliver: bool);
}

/// The engine surface a session needs, erased so consumers can hold a
/// [`VtTermSession`] without naming the engine's listener parameter.
pub trait SessionTerm: SessionRead {
    fn cursor_style_raw(&self) -> crate::model::CursorStyle;
    fn set_cursor_style_raw(&mut self, style: Option<crate::model::CursorStyle>);
    fn clear_row_wrap_raw(&mut self, line: i32);
    fn set_cursor_raw(&mut self, point: GridPoint, pending_wrap: bool);
    fn set_selection_raw(
        &mut self,
        kind: SelectionKind,
        from: GridPoint,
        from_side: Side,
        to: GridPoint,
        to_side: Side,
    );
    fn start_selection_raw(&mut self, kind: SelectionKind, point: GridPoint, side: Side);
    fn update_selection_raw(&mut self, point: GridPoint, side: Side);
    fn clear_selection_raw(&mut self);
    fn scroll_to_raw(&mut self, point: GridPoint);
    fn scroll_raw(&mut self, scroll: Scroll);
    fn clear_scrollback_raw(&mut self);
    fn resize_raw(&mut self, cols: usize, rows: usize);
    fn clear_below_raw(&mut self);
    fn linefeed_raw(&mut self);
    /// Begin tracking one cell — see [`crate::TrackedAnchor`]. `None` on
    /// an engine without tracking (the caller keeps its absolute-row
    /// anchor); creation is a read-and-register under the caller's lock.
    fn track_point_raw(&mut self, point: GridPoint) -> Option<crate::TrackedAnchor>;
}

/// A borrowed view over a live engine terminal: the session's surface minus
/// everything that writes. Over `&` rather than `&mut`, so a consumer that
/// only reads (the grid walkers, the composer's per-frame scan) takes its
/// lock shared — other readers keep running while it works. Constructed with
/// [`VtTermView::new`] at the call site and dropped right after, exactly like
/// the session.
pub struct VtTermView<'a> {
    term: &'a dyn SessionRead,
}

impl<'a> VtTermView<'a> {
    /// Over any live engine terminal — whichever backend compiled in.
    pub fn new(term: &'a dyn SessionRead) -> Self {
        Self { term }
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

    /// Scrollback rows above the live screen — the half of the grid's
    /// height the active area doesn't count.
    pub fn history_size(&self) -> usize {
        self.term.history_size_raw()
    }

    /// The bottom-most screen row's line address.
    pub fn bottommost_line(&self) -> i32 {
        self.term.bottommost_line_raw()
    }

    /// How far the viewport is scrolled up from the live screen, in rows:
    /// 0 means the live screen itself is showing.
    pub fn viewport_offset(&self) -> usize {
        self.term.viewport_offset_raw()
    }

    /// The cursor: position, visibility, pending wrap, which screen.
    pub fn cursor(&self) -> Cursor {
        self.term.cursor_raw()
    }

    /// The cursor's cell in viewport coordinates — the same lead-cell
    /// adjustment the frame's cursor carries, without the "inside the
    /// viewport" gate: a viewport scrolled into history still reports its
    /// cursor, in the row below the one a frame would paint. The input bar
    /// anchors there.
    ///
    /// The ungated row is a Phase 3 requirement, not an incidental shape: a
    /// render-state cursor that reports nothing once the cursor leaves the
    /// viewport (Ghostty's `render.h` one does) cannot back this read as
    /// is — the Ghostty backend derives the row from the active-area cursor
    /// plus the viewport offset instead.
    pub fn cursor_cell(&self) -> Option<(usize, usize)> {
        self.term.cursor_cell_raw()
    }

    /// One cell as a walker sees it: primary character, width class, OSC 8
    /// anchor. The point must lie inside the grid, as the walkers that
    /// bound themselves with [`Self::topmost_line`]/[`Self::columns`] do.
    pub fn glyph(&self, point: GridPoint) -> Glyph {
        self.term.glyph_raw(point)
    }

    /// One cell as the renderer sees it: full text, colors, style flags.
    /// [`Glyph`] is the letters; this is the styling. The point must lie
    /// inside the grid.
    pub fn cell(&self, point: GridPoint) -> Cell {
        self.term.cell_raw(point)
    }

    /// Whether the row soft-wrapped into the next one — no line break
    /// after it.
    pub fn row_wraps(&self, line: i32) -> bool {
        self.term.row_wraps_raw(line)
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

    /// Whether a selection is set at all — the copy gate's question, before
    /// resolution ([`Self::selection`] says `None` for one that resolves to
    /// no span).
    pub fn has_selection(&self) -> bool {
        self.term.has_selection_raw()
    }

    /// The selection's text, as the engine itself converts it (a `Lines`
    /// selection keeps whole rows and ends in a newline, and so on).
    pub fn selection_to_string(&self) -> Option<String> {
        self.term.selection_to_string_raw()
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

    /// Fill `frame` with the viewport a renderer paints (see [`Frame`]),
    /// reusing its buffers. Over the shared borrow a painter's lock gives:
    /// read-only, exactly the rest of this view.
    pub fn frame(&self, frame: &mut Frame, want_sliver: bool) {
        self.term.frame_raw(frame, want_sliver)
    }
}

/// A borrowed session over a live engine terminal, for the facade methods
/// that operate on a live engine handle (`command_cursor`'s restyling). Adds
/// no state; every method forwards. Constructed with [`VtTermSession::new`]
/// at the call site and dropped right after.
pub struct VtTermSession<'a> {
    term: &'a mut dyn SessionTerm,
}

impl<'a> VtTermSession<'a> {
    /// Over any live engine terminal — whichever backend compiled in.
    pub fn new(term: &'a mut dyn SessionTerm) -> Self {
        Self { term }
    }

    pub fn cursor_style(&self) -> crate::model::CursorStyle {
        self.term.cursor_style_raw()
    }

    /// Restyle the cursor, or with `None` hand it back to the configured
    /// default (what DECSCUSR `0 q` means).
    pub fn set_cursor_style(&mut self, style: Option<crate::model::CursorStyle>) {
        self.term.set_cursor_style_raw(style);
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

    /// Scrollback rows above the live screen — the half of the grid's
    /// height the active area doesn't count.
    pub fn history_size(&self) -> usize {
        self.term.history_size_raw()
    }

    /// The bottom-most screen row's line address.
    pub fn bottommost_line(&self) -> i32 {
        self.term.bottommost_line_raw()
    }

    /// How far the viewport is scrolled up from the live screen, in rows:
    /// 0 means the live screen itself is showing.
    pub fn viewport_offset(&self) -> usize {
        self.term.viewport_offset_raw()
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
    ///
    /// Backend contract: the alacritty op clears the flag alone, cell
    /// contents untouched. The Ghostty backend has no such engine op — it
    /// clears the flag by erasing the row's last cell — so it acts only
    /// when that cell is blank (the shell-padded rows this op's one caller,
    /// the prompt-reflow repair, brings) and otherwise leaves the row fully
    /// untouched, wrap kept, with a debug log naming the row.
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
        self.term.set_cursor_raw(point, pending_wrap)
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

    /// Whether a selection is set at all — the cheaper, earlier question
    /// than [`Self::selection`]: a gesture's first click sets a selection
    /// that resolves to no span yet, and a copy gate has to see it so the
    /// chord is consumed rather than falling through as a signal.
    pub fn has_selection(&self) -> bool {
        self.term.has_selection_raw()
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
        self.term
            .set_selection_raw(kind, from, from_side, to, to_side);
    }

    /// Begin a selection of `kind` at one cell edge — the anchor a drag's
    /// later updates move away from.
    pub fn start_selection(&mut self, kind: SelectionKind, point: GridPoint, side: Side) {
        self.term.start_selection_raw(kind, point, side);
    }

    /// Move the selection's free end to a new cell edge, leaving its anchor
    /// where it was. No-op when no selection is active.
    pub fn update_selection(&mut self, point: GridPoint, side: Side) {
        self.term.update_selection_raw(point, side);
    }

    /// Drop the active selection, if any.
    pub fn clear_selection(&mut self) {
        self.term.clear_selection_raw();
    }

    /// The minimal scroll that brings a grid point into the viewport —
    /// what revealing a search match wants.
    pub fn scroll_to(&mut self, point: GridPoint) {
        self.term.scroll_to_raw(point);
    }

    /// Move the viewport (see [`Scroll`]) — never the active area.
    pub fn scroll(&mut self, scroll: Scroll) {
        self.term.scroll_raw(scroll);
    }

    /// Drop every scrollback row (`CSI 3 J` as an engine op): the history
    /// goes, a selection anchored in it goes with it, and the viewport
    /// lands back on the live screen.
    pub fn clear_scrollback(&mut self) {
        self.term.clear_scrollback_raw();
    }

    /// Every match of `query` in the whole grid, oldest first, and
    /// whether the cap stopped the walk — the engine's own matcher driven
    /// by [`crate::VtSearch`], which owns the query dialect.
    pub(crate) fn search_all(&self, query: &SearchQuery, cap: usize) -> (Vec<SearchMatch>, bool) {
        self.term.search_all_raw(query, cap)
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

    /// Fill `frame` with the viewport a renderer paints (see [`Frame`]),
    /// reusing its buffers. Reads only; the caller holds whatever lock it
    /// holds for every other read.
    pub fn frame(&self, frame: &mut Frame, want_sliver: bool) {
        self.term.frame_raw(frame, want_sliver)
    }

    /// Resize the grid, reflowing the primary screen. A zero dimension
    /// becomes 1; nothing else is rounded (the alacritty backend's
    /// `MinSize` note).
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

    /// Begin tracking one cell: the returned anchor follows it through
    /// scrolling, pruning and reflow on engines that can, and `None`
    /// leaves the caller with the absolute-row anchor it already keeps
    /// (see [`crate::TrackedAnchor`]).
    pub fn track_point(&mut self, point: GridPoint) -> Option<crate::TrackedAnchor> {
        self.term.track_point_raw(point)
    }
}

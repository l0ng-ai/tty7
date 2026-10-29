//! The Ghostty backend (crate feature `ghostty`): libghostty-vt behind the
//! seam, the engine [`VtTerm`] names when the feature is on.
//!
//! [`Engine`] owns one C terminal plus the long-lived satellite handles the
//! C API designs to be created once and reused — the render state and its
//! iterators (`render.h`), and the search session (`search.h`). Everything
//! a caller receives is owned model data copied under the caller's lock;
//! no C pointer or borrowed row survives a call (the one rule the C headers
//! repeat about lifetimes). The trait impls live here so the borrowing
//! facades in [`crate::engine`] never name an engine.
//!
//! Where a seam operation has no C call, the backend speaks VT to its own
//! terminal: cursor moves, DECSCUSR restyles, `CSI 3 J`, linefeeds. That is
//! not a workaround so much as the engine's own language — the bytes are
//! exactly what a program would send — with one documented exception
//! ([`SessionTerm::clear_row_wrap_raw`], which no byte stream can express).

use std::ffi::c_void;
use std::time::Instant;

use tty59_vt_sys as sys;

use crate::engine::{sealed, SessionRead, SessionTerm};
use crate::event::{VtEvent, VtEventListener};
use crate::model::{
    Cell, CellWidth, Cursor, CursorShape, CursorStyle, Frame, Glyph, GridPoint, Modes, Rgb, Scroll,
    Screen, SearchMatch, SearchQuery, SearchStatus, Selection, SelectionKind, Side, Snapshot,
    VtOptions,
};
use crate::terminal::Terminal;

use sys::GhosttyCellData_GHOSTTY_CELL_DATA_HAS_HYPERLINK as CELL_HAS_HYPERLINK;
use sys::GhosttyCellData_GHOSTTY_CELL_DATA_HAS_TEXT as CELL_HAS_TEXT;
use sys::GhosttyCellData_GHOSTTY_CELL_DATA_WIDE as CELL_WIDE;
use sys::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN as TAG_SCREEN;
use sys::GhosttyRowData_GHOSTTY_ROW_DATA_WRAP as ROW_WRAP;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN as DATA_ACTIVE_SCREEN;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS as DATA_KITTY_FLAGS;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS as DATA_SCROLLBACK_ROWS;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SCROLLBAR as DATA_SCROLLBAR;
use sys::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_ALTERNATE as SCREEN_ALTERNATE;

// ---- Coordinate plumbing ---------------------------------------------------

/// A model grid point to the engine's own screen-coordinate point: `line`
/// counts 0-at-top-of-active-area with negatives into scrollback, the
/// screen tag counts 0-at-top-of-scrollback, so the scrollback depth is the
/// whole conversion.
fn screen_point(history: usize, point: GridPoint) -> sys::GhosttyPoint {
    sys::GhosttyPoint {
        tag: TAG_SCREEN,
        value: sys::GhosttyPointValue {
            coordinate: sys::GhosttyPointCoordinate {
                x: point.col,
                y: (history as i64 + point.line as i64) as u32,
            },
        },
    }
}

/// Resolve a model grid point to a C grid reference. `None` when the point
/// falls outside the grid — the callers bound themselves the way they did
/// against the engine being replaced, and a surprising miss must not panic
/// a read into a crash.
fn grid_ref(handle: sys::GhosttyTerminal, history: usize, point: GridPoint) -> Option<sys::GhosttyGridRef> {
    let mut r = sys::GhosttyGridRef {
        size: std::mem::size_of::<sys::GhosttyGridRef>(),
        node: std::ptr::null_mut(),
        x: 0,
        y: 0,
    };
    // SAFETY: a zeroed, size-initialized out-struct and a well-formed point;
    // failure is reported, not panicked.
    let ok = unsafe { sys::ghostty_terminal_grid_ref(handle, screen_point(history, point), &mut r) }
        == sys::GhosttyResult_GHOSTTY_SUCCESS;
    ok.then_some(r)
}

/// A resolved grid reference back to a model point, in the same screen
/// coordinates [`screen_point`] spells.
fn point_of(handle: sys::GhosttyTerminal, history: usize, r: &sys::GhosttyGridRef) -> Option<GridPoint> {
    let mut out = sys::GhosttyPointCoordinate { x: 0, y: 0 };
    // SAFETY: `r` is a live reference for this terminal; the out-struct
    // matches the documented type of the conversion.
    let ok = unsafe {
        sys::ghostty_terminal_point_from_grid_ref(handle, r, TAG_SCREEN, &mut out)
    } == sys::GhosttyResult_GHOSTTY_SUCCESS;
    ok.then_some(GridPoint {
        line: out.y as i64 as i32 - history as i32,
        col: out.x,
    })
}

/// One engine cell to the walker's view of it ([`Glyph`]).
fn glyph_of(r: &sys::GhosttyGridRef) -> Glyph {
    // SAFETY: `r` is a live grid reference; every get below passes the
    // output type its enum entry documents.
    unsafe {
        let mut cell: sys::GhosttyCell = 0;
        let mut has_text = false;
        let mut wide: sys::GhosttyCellWide = 0;
        if sys::ghostty_grid_ref_cell(r, &mut cell) == sys::GhosttyResult_GHOSTTY_SUCCESS {
            sys::ghostty_cell_get(cell, CELL_HAS_TEXT, &mut has_text as *mut _ as *mut c_void);
            sys::ghostty_cell_get(cell, CELL_WIDE, &mut wide as *mut _ as *mut c_void);
        }
        // Unwritten cells read as spaces — the model's contract, and the
        // engine being replaced's storage — because grid walkers ask "is
        // this cell occupied" by comparing the character.
        let c = if has_text {
            crate::snapshot::grapheme_cluster(r)
                .first()
                .copied()
                .unwrap_or(' ')
        } else {
            ' '
        };
        Glyph {
            c,
            wide: width_of(wide),
            hyperlink: if has_hyperlink(r) {
                Some(crate::snapshot::hyperlink_uri(r))
            } else {
                None
            },
        }
    }
}

fn width_of(wide: sys::GhosttyCellWide) -> CellWidth {
    use sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_HEAD as HEAD;
    use sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_TAIL as TAIL;
    use sys::GhosttyCellWide_GHOSTTY_CELL_WIDE_WIDE as WIDE;
    match wide {
        x if x == WIDE => CellWidth::Wide,
        x if x == TAIL => CellWidth::SpacerTail,
        x if x == HEAD => CellWidth::SpacerHead,
        _ => CellWidth::Narrow,
    }
}

/// A cell's combining marks, beside its base character (the walker's
/// cluster join, split for the text matcher).
fn marks_of(r: &sys::GhosttyGridRef) -> smallvec::SmallVec<[char; 2]> {
    let mut cluster = crate::snapshot::grapheme_cluster(r);
    if !cluster.is_empty() {
        cluster.remove(0);
    }
    cluster
}

/// Regex metacharacter escaping for a literal query — the same set the
/// alacritty backend's matcher escapes (its `regex_escape`), so a literal
/// on either backend spells the same pattern.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
                | '#' | '&' | '-' | '~'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A raw-read cell in the seam's vocabulary: the wrapper's no-text marker
/// (`'\0'`, which its own snapshot pins) becomes the space the model
/// promises for an unwritten cell. Spacer cells keep `\0` — no text, no
/// glyph, that is their whole meaning.
fn seam_cell(mut cell: Cell) -> Cell {
    if cell.c == '\0' && cell.wide == CellWidth::Narrow {
        cell.c = ' ';
    }
    cell
}

/// Whether the cell at `r` anchors an OSC 8 link.
fn has_hyperlink(r: &sys::GhosttyGridRef) -> bool {
    let mut cell: sys::GhosttyCell = 0;
    let mut has = false;
    // SAFETY: `r` is live; the out-params match the documented types.
    unsafe {
        if sys::ghostty_grid_ref_cell(r, &mut cell) == sys::GhosttyResult_GHOSTTY_SUCCESS {
            sys::ghostty_cell_get(cell, CELL_HAS_HYPERLINK, &mut has as *mut _ as *mut c_void);
        }
    }
    has
}

// ---- The engine ------------------------------------------------------------

/// The state one Ghostty-backed terminal carries between calls.
///
/// The selection bookkeeping is wrapper-side because the C selection API
/// answers *where* a selection is, not *how it was made*: the kind and the
/// gesture state (anchor, whether a drag's first click has resolved yet)
/// are what the seam's contracts pin, so they are tracked where the writes
/// happen and joined with the engine's span on every read.
pub(crate) struct Engine {
    pub(crate) term: Terminal,
    pub(crate) render: RenderHandles,
    /// The ED2 parity rewriter and its staging buffer (Phase 4): `CSI 2 J`
    /// on the primary screen is spliced into the engine's own
    /// scroll-clear before the stream reaches the parser.
    pub(crate) ed2: crate::ghostty_ed2::Ed2Rewriter,
    pub(crate) ed2_buf: Vec<u8>,
    search: sys::GhosttySearch,
    options: VtOptions,
    /// The gesture state behind [`SessionRead::selection_raw`] and friends.
    selection: SelectionState,
    /// The frame fill's generation counter — see [`Frame`] and
    /// `frame_raw`. A `Cell` because the fill runs through the read-only
    /// trait while holding the caller's exclusive lock.
    pub(crate) generation: std::cell::Cell<u64>,
}

/// What the mouse-gesture surface tracks about a selection the engine holds.
struct SelectionState {
    kind: SelectionKind,
    anchor: Option<(GridPoint, Side)>,
    /// A `start_selection` whose first `update_selection` has not landed:
    /// set, but covering no cells — the copy gate sees it, text extraction
    /// does not (the seam's Phase 2f-1 contract).
    unresolved: bool,
}

impl Default for SelectionState {
    fn default() -> Self {
        Self {
            kind: SelectionKind::Simple,
            anchor: None,
            unresolved: false,
        }
    }
}

impl Engine {
    fn new(options: VtOptions, cols: u16, rows: u16) -> Result<Self, crate::terminal::Error> {
        // Zero dimensions are raised to 1 — the same floor the alacritty
        // backend's `MinSize` applies, for the same reason: a caller with no
        // reported size must still get a working grid.
        let term = Terminal::new(cols.max(1), rows.max(1), options.scrollback_lines)?;
        let handle = term.handle();
        let render = RenderHandles::new(handle)?;
        let mut search: sys::GhosttySearch = std::ptr::null_mut();
        // SAFETY: a valid out-pointer and a live terminal handle; creation
        // reads nothing and registers the search for two-way free.
        let r = unsafe { sys::ghostty_search_new(std::ptr::null(), &mut search, handle) };
        if r != sys::GhosttyResult_GHOSTTY_SUCCESS {
            return Err(crate::terminal::Error::new(r));
        }
        let mut engine = Self {
            term,
            render,
            ed2: crate::ghostty_ed2::Ed2Rewriter::default(),
            ed2_buf: Vec::new(),
            search,
            options,
            selection: SelectionState::default(),
            generation: std::cell::Cell::new(0),
        };
        // Hold-begins re-capture the render state from inside the callback
        // (see the trampoline): the frame as of the BSU stays on screen.
        engine.term.set_render_capture(engine.render.state);
        engine.apply_options();
        Ok(engine)
    }

    /// Map the model's options onto the engine's own. Called at
    /// construction and on `set_options`; only the mappable fields go, and
    /// every field the model names lands somewhere or is a named gap (see
    /// the module docs and the migration doc's Phase 3 notes).
    fn apply_options(&mut self) {
        let options = self.options.clone();
        let handle = self.term.handle();
        // SAFETY: every set below passes the documented value type for its
        // option; failures are surfaced rather than swallowed.
        unsafe {
            let cursor = match options.cursor_style.shape {
                CursorShape::Block => {
                    sys::GhosttyTerminalCursorStyle_GHOSTTY_TERMINAL_CURSOR_STYLE_BLOCK
                }
                CursorShape::Beam => {
                    sys::GhosttyTerminalCursorStyle_GHOSTTY_TERMINAL_CURSOR_STYLE_BAR
                }
                CursorShape::Underline | CursorShape::HollowBlock => {
                    sys::GhosttyTerminalCursorStyle_GHOSTTY_TERMINAL_CURSOR_STYLE_UNDERLINE
                }
            };
            self.term
                .set_raw(
                    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_STYLE,
                    &cursor as *const _ as *const c_void,
                )
                .expect("the cursor style is a valid option value");
            self.term
                .set_raw(
                    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_BLINK,
                    &options.cursor_style.blinking as *const _ as *const c_void,
                )
                .expect("the cursor blink is a valid option value");

            // The theme, so OSC 4/10/11/12 replies carry what the embedder
            // draws (the engine answers those itself — there is no query
            // callback to route them through).
            let color = |c: Rgb| sys::GhosttyColorRgb { r: c.r, g: c.g, b: c.b };
            let fg = color(options.colors.foreground);
            let bg = color(options.colors.background);
            let cursor_color = color(options.colors.cursor);
            let mut palette = [sys::GhosttyColorRgb { r: 0, g: 0, b: 0 }; 256];
            for (slot, c) in palette.iter_mut().zip(options.colors.palette) {
                *slot = color(c);
            }
            self.term
                .set_raw(
                    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
                    &fg as *const _ as *const c_void,
                )
                .expect("a valid color option value");
            self.term
                .set_raw(
                    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
                    &bg as *const _ as *const c_void,
                )
                .expect("a valid color option value");
            self.term
                .set_raw(
                    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
                    &cursor_color as *const _ as *const c_void,
                )
                .expect("a valid color option value");
            self.term
                .set_raw(
                    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_COLOR_PALETTE,
                    &palette as *const _ as *const c_void,
                )
                .expect("a valid color option value");

            // Exactly one image path: the daemon lifts APC/Kitty frames out
            // of the stream (decision 5), so the engine's own handling is
            // capped at zero bytes.
            let zero: usize = 0;
            self.term
                .set_raw(
                    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_APC_MAX_BYTES,
                    &zero as *const _ as *const c_void,
                )
                .expect("a valid APC limit");
            self.term
                .set_raw(
                    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_APC_MAX_BYTES_KITTY,
                    &zero as *const _ as *const c_void,
                )
                .expect("a valid Kitty APC limit");
            self.term
                .set_raw(
                    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT,
                    &zero as *const usize as *const c_void,
                )
                .expect("a valid Kitty storage limit");
            let _ = handle;
        }
        // `kitty_keyboard` and `conpty_resize` have no mapping here: the
        // kitty protocol is always spoken (an intended difference — see the
        // differential harness's allow-list), and conhost re-anchoring is
        // Phase 5's work, not an option this engine takes yet.
    }

    pub(crate) fn history(&self) -> usize {
        let mut rows: usize = 0;
        // SAFETY: `usize*` is the documented output type.
        unsafe {
            sys::ghostty_terminal_get(
                self.term.handle(),
                DATA_SCROLLBACK_ROWS,
                &mut rows as *mut _ as *mut c_void,
            )
        };
        rows
    }

    /// The viewport's distance from the live screen, in rows — the engine
    /// reports a scrollbar (`{total, offset, len}` from the top of the
    /// scrollable area), which is the same fact from the other end.
    pub(crate) fn display_offset(&self) -> usize {
        let mut bar = sys::GhosttyTerminalScrollbar { total: 0, offset: 0, len: 0 };
        // SAFETY: `GhosttyTerminalScrollbar*` is the documented output type.
        unsafe {
            sys::ghostty_terminal_get(
                self.term.handle(),
                DATA_SCROLLBAR,
                &mut bar as *mut _ as *mut c_void,
            )
        };
        (bar.total.saturating_sub(bar.len).saturating_sub(bar.offset)) as usize
    }

    pub(crate) fn alt_screen(&self) -> bool {
        let mut screen: sys::GhosttyTerminalScreen = 0;
        // SAFETY: the enum's own type is the documented output.
        unsafe {
            sys::ghostty_terminal_get(
                self.term.handle(),
                DATA_ACTIVE_SCREEN,
                &mut screen as *mut _ as *mut c_void,
            )
        };
        screen == SCREEN_ALTERNATE
    }

    /// Speak VT to the engine — the writes that have no dedicated call.
    /// Effects stay armed (a DECSCUSR fires none), so a plain `&mut` write
    /// through the wrapper is the whole story.
    fn speak(&mut self, bytes: &[u8]) {
        self.term.write(bytes);
    }

    /// The active selection as the seam spells it: the engine's span, the
    /// gesture's kind, unresolved gesture clicks gated to `None`.
    pub(crate) fn selection(&self) -> Option<Selection> {
        if self.selection.unresolved {
            return None;
        }
        let sel = self.raw_selection()?;
        Some(Selection {
            kind: self.selection.kind,
            span: sel,
        })
    }

    /// The engine's active selection span, resolved to model points and
    /// normalized top-left first. `None` when none is installed.
    fn raw_selection(&self) -> Option<crate::model::SelectionSpan> {
        let handle = self.term.handle();
        let history = self.history();
        let mut sel = sys::GhosttySelection {
            size: std::mem::size_of::<sys::GhosttySelection>(),
            start: unsafe { std::mem::zeroed() },
            end: unsafe { std::mem::zeroed() },
            rectangle: false,
        };
        // SAFETY: `GhosttySelection*` is the documented output; the refs it
        // writes are used below before any mutation.
        if unsafe { sys::ghostty_terminal_get(handle, DATA_SELECTION_CODE, &mut sel as *mut _ as *mut c_void) }
            != sys::GhosttyResult_GHOSTTY_SUCCESS
        {
            return None;
        }
        let start = point_of(handle, history, &sel.start)?;
        let end = point_of(handle, history, &sel.end)?;
        let (start, end) = if (start.line, start.col) <= (end.line, end.col) {
            (start, end)
        } else {
            (end, start)
        };
        Some(crate::model::SelectionSpan { start, end })
    }

    /// The word selection around `point` — selection.h's own walker, bounded
    /// by the configured separators (`word_separators` through `VtOptions`),
    /// exactly the boundaries the replaced engine expanded a `Semantic`
    /// gesture to. `None` off-grid; `None` on a cell with no selectable
    /// word (whitespace), where the caller falls back to the raw point.
    fn word_span(&self, point: GridPoint) -> Option<(GridPoint, GridPoint)> {
        let handle = self.term.handle();
        let history = self.history();
        let r = grid_ref(handle, history, point)?;
        let boundaries: Vec<u32> =
            self.options.semantic_escape_chars.chars().map(u32::from).collect();
        let options = sys::GhosttyTerminalSelectWordOptions {
            size: std::mem::size_of::<sys::GhosttyTerminalSelectWordOptions>(),
            ref_: r,
            boundary_codepoints: if boundaries.is_empty() {
                std::ptr::null()
            } else {
                boundaries.as_ptr()
            },
            boundary_codepoints_len: boundaries.len(),
        };
        let mut sel = sys::GhosttySelection {
            size: std::mem::size_of::<sys::GhosttySelection>(),
            start: unsafe { std::mem::zeroed() },
            end: unsafe { std::mem::zeroed() },
            rectangle: false,
        };
        // SAFETY: `r` is a live ref from this terminal with no mutation in
        // between; the options carry the documented types, and the out
        // selection is read below before any mutation.
        let found = unsafe { sys::ghostty_terminal_select_word(handle, &options, &mut sel) }
            == sys::GhosttyResult_GHOSTTY_SUCCESS;
        if !found {
            return None;
        }
        let start = point_of(handle, history, &sel.start)?;
        let end = point_of(handle, history, &sel.end)?;
        Some(if (start.line, start.col) <= (end.line, end.col) {
            (start, end)
        } else {
            (end, start)
        })
    }

    /// Install a selection between two cell edges. Sides live in the seam's
    /// vocabulary; the engine's refs are cell-granular, so a `Right` edge at
    /// column 0 — the alacritty spelling of "through the end of the
    /// previous row" — is translated to that row's last column rather than
    /// lost.
    fn install_selection(&mut self, kind: SelectionKind, from: (GridPoint, Side), to: (GridPoint, Side)) {
        let cols = self.term.size().0;
        // Whole-row selections are spanned whole-row: the engine's formatter
        // converts the span it is given, and a `Lines` gesture means every
        // row between its endpoints, not the columns they happen to name.
        if matches!(kind, SelectionKind::Lines) {
            let from = GridPoint { col: 0, ..from.0 };
            let to = GridPoint { col: cols.saturating_sub(1), ..to.0 };
            return self.install_selection_span(kind, from, to);
        }
        // Word selections expand at both ends — the anchor locks to its
        // word, the free end to whatever word it lands in, which is the
        // drag the `Semantic` kind names on the engine being replaced. A
        // cell with no selectable word (whitespace) keeps the raw point,
        // the way a word-wise drag through the padding selects nothing new.
        if matches!(kind, SelectionKind::Semantic) {
            let from_word = self.word_span(from.0);
            let to_word = self.word_span(to.0);
            if let (Some((ws, _)), Some((_, we))) = (from_word, to_word) {
                return self.install_selection_span(kind, ws, we);
            }
        }
        let adjust = |(point, side): (GridPoint, Side)| -> (GridPoint, Side) {
            match side {
                Side::Left => (point, side),
                Side::Right if point.col == 0 && point.line > from.0.line => (
                    GridPoint { line: point.line - 1, col: cols.saturating_sub(1) },
                    Side::Right,
                ),
                Side::Right => (point, side),
            }
        };
        let (from, _) = adjust(from);
        let (to, _) = adjust(to);
        self.install_selection_span(kind, from, to)
    }

    /// Install a selection already normalized to cell coordinates.
    fn install_selection_span(&mut self, kind: SelectionKind, from: GridPoint, to: GridPoint) {
        let history = self.history();
        let rectangle = matches!(kind, SelectionKind::Block);
        let handle = self.term.handle();
        let (Some(start), Some(end)) = (
            grid_ref(handle, history, from),
            grid_ref(handle, history, to),
        ) else {
            // Outside the grid: the engine holds nothing, which is the
            // honest answer for a selection that names no cells.
            self.clear_engine_selection();
            return;
        };
        let sel = sys::GhosttySelection {
            size: std::mem::size_of::<sys::GhosttySelection>(),
            start,
            end,
            rectangle,
        };
        // SAFETY: the refs were just resolved from this terminal and no
        // mutating call has run since; `GhosttySelection*` is the
        // documented input type of the option.
        unsafe {
            self.term
                .set_raw(
                    sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SELECTION,
                    &sel as *const _ as *const c_void,
                )
                .expect("a live selection is a valid option value");
        }
    }

    fn clear_engine_selection(&mut self) {
        // SAFETY: NULL is the documented "clear the selection" value.
        unsafe {
            self.term
                .set_raw(sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SELECTION, std::ptr::null())
                .expect("clearing the selection cannot fail");
        }
    }

    /// The whole grid as an owned snapshot: scrollback rows first, then the
    /// screen, through the same per-cell reads the viewport snapshot takes.
    fn snapshot_full(&self) -> Snapshot {
        let handle = self.term.handle();
        let cols = self.term.size().0;
        let rows = self.term.size().1;
        let history = self.history();
        let mut hyperlinks = Vec::new();
        let mut out = Vec::with_capacity(history + rows as usize);
        for line in -(history as i32)..rows as i32 {
            let mut cells = Vec::with_capacity(cols as usize);
            let mut wrap = false;
            for col in 0..cols {
                let point = GridPoint { line, col };
                let Some(r) = grid_ref(handle, history, point) else {
                    cells.push(Cell::default());
                    continue;
                };
                if col == 0 {
                    wrap = self.row_wraps(line);
                }
                cells.push(seam_cell(crate::snapshot::read_cell(&r, &mut hyperlinks)));
            }
            out.push(crate::model::Row {
                cells,
                wrap,
                wrap_continuation: false,
                dirty: false,
            });
        }
        let cursor = self.cursor();
        Snapshot {
            cols,
            screen_rows: rows,
            history_rows: history,
            cursor,
            modes: self.modes(),
            hyperlinks,
            rows: out,
        }
    }

    fn cursor(&self) -> Cursor {
        let cursor = self.term.cursor();
        Cursor {
            x: cursor.x,
            y: cursor.y,
            visible: cursor.visible,
            pending_wrap: cursor.pending_wrap,
            screen: cursor.screen,
        }
    }

    fn row_wraps(&self, line: i32) -> bool {
        let history = self.history();
        let Some(r) = grid_ref(self.term.handle(), history, GridPoint { line, col: 0 }) else {
            return false;
        };
        let mut row: sys::GhosttyRow = 0;
        let mut wrap = false;
        // SAFETY: `r` is live for this read; both out-params match their
        // documented types.
        unsafe {
            if sys::ghostty_grid_ref_row(&r, &mut row) == sys::GhosttyResult_GHOSTTY_SUCCESS {
                sys::ghostty_row_get(row, ROW_WRAP, &mut wrap as *mut _ as *mut c_void);
            }
        }
        wrap
    }

    /// The mode bits the model knows, read one mode at a time — the C API
    /// has no bulk form. Cheap calls, off the frame path.
    fn modes(&self) -> Modes {
        use crate::terminal::Mode;
        let handle = self.term.handle();
        let mut out = Modes::empty();
        let dec = |number: u16| {
            let mut cfg = sys::GhosttyTerminalModeConfig { mode: Mode::dec(number).0, value: false };
            // SAFETY: `GhosttyTerminalModeConfig*` is the documented
            // in/out type, with `mode` initialized.
            unsafe {
                sys::ghostty_terminal_get(handle, DATA_MODE_CODE, &mut cfg as *mut _ as *mut c_void);
            }
            cfg.value
        };
        out.set(Modes::WRAPAROUND, dec(7));
        out.set(Modes::APP_CURSOR, dec(1));
        out.set(Modes::APP_KEYPAD, dec(66));
        out.set(Modes::SHOW_CURSOR, dec(25));
        out.set(Modes::BRACKETED_PASTE, dec(2004));
        out.set(Modes::FOCUS_IN_OUT, dec(1004));
        out.set(Modes::MOUSE_REPORT_CLICK, dec(1000));
        out.set(Modes::MOUSE_DRAG, dec(1002));
        out.set(Modes::MOUSE_MOTION, dec(1003));
        out.set(Modes::SGR_MOUSE, dec(1006));
        out.set(Modes::UTF8_MOUSE, dec(1005));
        out.set(Modes::ORIGIN, dec(6));
        out.set(Modes::ALTERNATE_SCROLL, dec(1007));
        // Which buffer is showing is one engine fact, however many DEC mode
        // numbers spell "the alternate screen" — this reads the fact.
        out.set(Modes::ALT_SCREEN, self.alt_screen());
        // The kitty keyboard protocol's progressive-enhancement flags: the
        // engine keeps them as one bitfield, the model names the bits.
        let mut flags: u8 = 0;
        // SAFETY: `u8*` is the documented output type.
        unsafe {
            sys::ghostty_terminal_get(handle, DATA_KITTY_FLAGS, &mut flags as *mut _ as *mut c_void);
        }
        out.set(Modes::KITTY_DISAMBIGUATE, flags & 1 != 0);
        out.set(Modes::KITTY_REPORT_EVENT_TYPES, flags & 2 != 0);
        out.set(Modes::KITTY_REPORT_ALTERNATE_KEYS, flags & 4 != 0);
        out.set(Modes::KITTY_REPORT_ALL_KEYS, flags & 8 != 0);
        out.set(Modes::KITTY_REPORT_TEXT, flags & 16 != 0);
        out
    }

    /// The regex/case-sensitive half of the search (decision 6): the grid's
    /// text, one logical line per soft-wrapped run, with a map from every
    /// character back to the cell that holds it. The pattern runs over
    /// that; matches land as grid spans the way the replaced engine's
    /// walker produced them — oldest first, inclusive ends, wide glyphs
    /// anchored at their lead cell.
    fn search_all_text(&self, query: &SearchQuery, cap: usize) -> (Vec<SearchMatch>, bool) {
        let pattern = match query.regex {
            true => query.text.clone(),
            false => regex_escape(&query.text),
        };
        let Ok(re) = regex::RegexBuilder::new(&pattern)
            .case_insensitive(!query.case_sensitive)
            .build()
        else {
            return (Vec::new(), false);
        };

        // One pass over the grid building the logical lines: wrapped rows
        // concatenate (a match may span the wrap, as the replaced engine's
        // did), hard line ends separate with a newline no plain pattern
        // crosses. Every character remembers the cell it came from.
        let handle = self.term.handle();
        let history = self.history();
        let cols = self.columns_raw() as u16;
        let mut text = String::new();
        // (line, col) of each character in `text`, `None` for the hard-line
        // newlines the matcher must not map.
        let mut map: Vec<Option<GridPoint>> = Vec::new();
        let topmost = -(history as i32);
        let bottom = self.screen_lines_raw() as i32 - 1;
        let mut line = topmost;
        while line <= bottom {
            let mut col = 0u16;
            while col < cols {
                let point = GridPoint { line, col };
                let Some(r) = grid_ref(handle, history, point) else {
                    map.push(Some(point));
                    text.push(' ');
                    col += 1;
                    continue;
                };
                let glyph = glyph_of(&r);
                match glyph.wide {
                    CellWidth::SpacerTail | CellWidth::SpacerHead => {
                        // No character of its own; the wide glyph it belongs
                        // to already mapped both columns' worth of text.
                        col += 1;
                        continue;
                    }
                    _ => {
                        // The base character plus any combining marks all
                        // live in this cell — the replaced engine's walker
                        // matched over the same joined text.
                        map.push(Some(point));
                        text.push(if glyph.c == '\0' { ' ' } else { glyph.c });
                        for mark in marks_of(&r) {
                            map.push(Some(point));
                            text.push(mark);
                        }
                        col += 1;
                    }
                }
            }
            if line < bottom && !self.row_wraps(line) {
                map.push(None);
                text.push('\n');
            }
            line += 1;
        }

        let mut out = Vec::new();
        let mut capped = false;
        for mat in re.find_iter(&text) {
            // The regex crate reports byte offsets; the map is per
            // character (a wide glyph or a combining mark is one entry per
            // character, and CJK is three bytes to the regex's one entry).
            let start_char = text[..mat.start()].chars().count();
            let end_char = start_char + mat.as_str().chars().count() - 1;
            // A match that touches a hard-line separator (or the ends of
            // the grid) is not a grid span; the replaced engine's walker
            // could not produce one either.
            let (Some(start), Some(end)) = (
                map.get(start_char).copied().flatten(),
                map.get(end_char).copied().flatten(),
            ) else {
                continue;
            };
            if out.len() >= cap {
                capped = true;
                break;
            }
            let (start, end) = if (start.line, start.col) <= (end.line, end.col) {
                (start, end)
            } else {
                (end, start)
            };
            out.push(SearchMatch { start, end });
        }
        (out, capped)
    }
}

use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_MODE as DATA_MODE_CODE;
use sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SELECTION as DATA_SELECTION_CODE;

/// The long-lived render-state handles and their iterators (`render.h`):
/// created once, updated per frame fill, never freed until the engine drops.
pub(crate) struct RenderHandles {
    pub(crate) state: sys::GhosttyRenderState,
    pub(crate) rows: sys::GhosttyRenderStateRowIterator,
    pub(crate) cells: sys::GhosttyRenderStateRowCells,
    /// The overscan the next update will capture: a row above the viewport
    /// when the last fill asked for a sliver, none otherwise. A `Cell`
    /// because the fill runs through the read-only trait under the caller's
    /// exclusive lock.
    overscan_above: std::cell::Cell<u16>,
}


impl RenderHandles {
    fn new(handle: sys::GhosttyTerminal) -> Result<Self, crate::terminal::Error> {
        let mut state: sys::GhosttyRenderState = std::ptr::null_mut();
        let mut rows: sys::GhosttyRenderStateRowIterator = std::ptr::null_mut();
        let mut cells: sys::GhosttyRenderStateRowCells = std::ptr::null_mut();
        // SAFETY: valid out-pointers; allocation failure is the only error,
        // mapped below.
        unsafe {
            let r = sys::ghostty_render_state_new(std::ptr::null(), &mut state);
            if r != sys::GhosttyResult_GHOSTTY_SUCCESS {
                return Err(crate::terminal::Error::new(r));
            }
            let r = sys::ghostty_render_state_row_iterator_new(std::ptr::null(), &mut rows);
            if r != sys::GhosttyResult_GHOSTTY_SUCCESS {
                return Err(crate::terminal::Error::new(r));
            }
            let r = sys::ghostty_render_state_row_cells_new(std::ptr::null(), &mut cells);
            if r != sys::GhosttyResult_GHOSTTY_SUCCESS {
                return Err(crate::terminal::Error::new(r));
            }
            // The render state needs no terminal at construction; the first
            // update binds them for good. One touch here so a broken
            // handle fails at construction, not mid-frame.
            let r = sys::ghostty_render_state_update(state, handle);
            if r != sys::GhosttyResult_GHOSTTY_SUCCESS {
                return Err(crate::terminal::Error::new(r));
            }
            sys::ghostty_render_state_clean(state);
        }
        Ok(Self {
            state,
            rows,
            cells,
            overscan_above: std::cell::Cell::new(0),
        })
    }

    /// Set the overscan request for the next update, when it changed.
    pub(crate) fn set_overscan(&self, above: u16) {
        if self.overscan_above.get() == above {
            return;
        }
        let request = sys::GhosttyRenderStateOverscan { above, below: 0 };
        // SAFETY: `GhosttyRenderStateOverscan*` is the documented input.
        unsafe {
            sys::ghostty_render_state_set(
                self.state,
                sys::GhosttyRenderStateOption_GHOSTTY_RENDER_STATE_OPTION_OVERSCAN,
                &request as *const _ as *const c_void,
            );
        }
        self.overscan_above.set(above);
    }
}

impl Drop for RenderHandles {
    fn drop(&mut self) {
        // SAFETY: all three handles are live and owned; freeing them in any
        // order is allowed.
        unsafe {
            sys::ghostty_render_state_row_cells_free(self.cells);
            sys::ghostty_render_state_row_iterator_free(self.rows);
            sys::ghostty_render_state_free(self.state);
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: the search borrows this terminal and both are alive here;
        // free the borrower first.
        unsafe { sys::ghostty_search_free(self.search) };
    }
}


// ---- SessionRead -----------------------------------------------------------

impl sealed::Sealed for Engine {}

impl SessionRead for Engine {
    fn columns_raw(&self) -> usize {
        usize::from(self.term.size().0)
    }

    fn screen_lines_raw(&self) -> usize {
        usize::from(self.term.size().1)
    }

    fn history_size_raw(&self) -> usize {
        self.history()
    }

    fn topmost_line_raw(&self) -> i32 {
        -(self.history() as i32)
    }

    fn bottommost_line_raw(&self) -> i32 {
        self.screen_lines_raw() as i32 - 1
    }

    fn viewport_offset_raw(&self) -> usize {
        self.display_offset()
    }

    fn cursor_raw(&self) -> Cursor {
        self.cursor()
    }

    fn glyph_raw(&self, point: GridPoint) -> Glyph {
        grid_ref(self.term.handle(), self.history(), point)
            .map(|r| glyph_of(&r))
            .unwrap_or(Glyph { c: '\0', wide: CellWidth::Narrow, hyperlink: None })
    }

    fn cell_raw(&self, point: GridPoint) -> Cell {
        let mut links = Vec::new();
        grid_ref(self.term.handle(), self.history(), point)
            .map(|r| seam_cell(crate::snapshot::read_cell(&r, &mut links)))
            .unwrap_or_default()
    }

    fn row_wraps_raw(&self, line: i32) -> bool {
        self.row_wraps(line)
    }

    fn text_raw(&self, from: GridPoint, to: GridPoint) -> String {
        // The engine being replaced's bounds extraction: cells from `from`
        // to `to` inclusive, soft-wrapped rows joined with no break, hard
        // line ends broken, unwritten interior cells reading as the spaces
        // the old grid stored — and unwritten *trailing* cells as nothing,
        // the way its `bounds_to_string` dropped each row's padding.
        let history = self.history();
        let cols = self.columns_raw() as u16;
        let handle = self.term.handle();
        let mut out = String::new();
        let mut line = from.line;
        while line <= to.line {
            let last_col = if line == to.line { to.col } else { cols.saturating_sub(1) };
            let first_col = if line == from.line { from.col } else { 0 };
            let start = out.len();
            for col in first_col..=last_col {
                let Some(r) = grid_ref(handle, history, GridPoint { line, col }) else {
                    continue;
                };
                let glyph = glyph_of(&r);
                match glyph.wide {
                    CellWidth::SpacerTail | CellWidth::SpacerHead => continue,
                    _ => out.push(if glyph.c == '\0' { ' ' } else { glyph.c }),
                }
            }
            // The row's tail padding: not spaces the program wrote, just
            // the rectangle showing. (A wrapped row is fully occupied, so
            // this trim only ever touches padding.)
            let trimmed = out[start..].trim_end_matches(' ').len();
            out.truncate(start + trimmed);
            if line < to.line && !self.row_wraps(line) {
                out.push('\n');
            }
            line += 1;
        }
        out
    }

    fn selection_raw(&self) -> Option<Selection> {
        self.selection()
    }

    fn has_selection_raw(&self) -> bool {
        // The gesture's question, answered from the gesture's state: a
        // click that has not resolved yet still *set* one. Once resolved,
        // the engine's own selection is asked to agree — a purge (or any
        // mutation that empties it) ends the gesture's selection too, the
        // way the engine being replaced dropped the object outright.
        self.selection.anchor.is_some()
            && (self.selection.unresolved || self.raw_selection().is_some())
    }

    fn selection_to_string_raw(&self) -> Option<String> {
        if self.selection.unresolved {
            return None;
        }
        let handle = self.term.handle();
        let options = sys::GhosttyTerminalSelectionFormatOptions {
            size: std::mem::size_of::<sys::GhosttyTerminalSelectionFormatOptions>(),
            emit: sys::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_PLAIN,
            // Unwrap joins soft-wrapped rows and trim drops the padding a
            // rectangle pads in — the same conversions the replaced engine
            // applied while building its selection strings.
            unwrap: true,
            trim: true,
            selection: std::ptr::null(),
        };
        let mut len = 0usize;
        // SAFETY: the size-query form is the documented way to learn the
        // output length; options carry the documented types.
        let r = unsafe {
            sys::ghostty_terminal_selection_format_buf(
                handle,
                options,
                std::ptr::null_mut(),
                0,
                &mut len,
            )
        };
        if r == sys::GhosttyResult_GHOSTTY_NO_VALUE {
            return None;
        }
        let mut buf = vec![0u8; len];
        // SAFETY: `buf` is exactly the size the first call asked for.
        let r = unsafe {
            sys::ghostty_terminal_selection_format_buf(
                handle,
                options,
                buf.as_mut_ptr(),
                buf.len(),
                &mut len,
            )
        };
        if r != sys::GhosttyResult_GHOSTTY_SUCCESS {
            return None;
        }
        buf.truncate(len);
        let mut text = String::from_utf8_lossy(&buf).into_owned();
        // The engine's formatter trims the trailing blank rows a selection
        // can cover; the converter being replaced emitted a part for every
        // *hard* line break in the span — soft-wrapped rows joining, blank
        // rows keeping their empty part — and consumers (the copy-unwrap
        // rejoin) count parts against exactly that. Pad the missing blank
        // parts back.
        if let Some(span) = self.raw_selection() {
            let breaks = (span.start.line..span.end.line)
                .filter(|&line| !self.row_wraps(line))
                .count();
            let need = breaks + 1;
            while text.split('\n').count() < need {
                text.push('\n');
            }
        }
        // The seam's contract for whole-row selections: they end in a
        // newline (see the model's `SelectionKind::Lines`). The engine's
        // formatter stops at the last selected cell, so the final row break
        // is the gesture's to add back.
        if matches!(self.selection.kind, SelectionKind::Lines) && !text.ends_with('\n') {
            text.push('\n');
        }
        Some(text)
    }

    fn modes_raw(&self) -> Modes {
        self.modes()
    }

    fn semantic_escape_chars_raw(&self) -> &str {
        &self.options.semantic_escape_chars
    }

    fn search_all_raw(&self, query: &SearchQuery, cap: usize) -> (Vec<SearchMatch>, bool) {
        // Plain case-insensitive queries are search.h's own session (set
        // the needle, run to completion — the blocking form, the seam's
        // feed being a one-shot rescan — then read the matches newest-first
        // and flip them into the grid order the seam promises). Regex and
        // case-sensitive queries are not expressible as a needle, so they
        // run through the text matcher (decision 6's hybrid: tty59's
        // pattern over the grid's text, mapped back to cells).
        if query.regex || query.case_sensitive {
            return self.search_all_text(query, cap);
        }
        let handle = self.term.handle();
        let needle = sys::GhosttyString {
            ptr: query.text.as_ptr(),
            len: query.text.len(),
        };
        // SAFETY: the needle is copied on set, so the borrow ends with the
        // call; run and get take live handles.
        unsafe {
            sys::ghostty_search_set(
                self.search,
                sys::GhosttySearchOption_GHOSTTY_SEARCH_OPT_NEEDLE,
                &needle as *const _ as *const c_void,
            );
            sys::ghostty_search_run(self.search);
        }
        let mut buffer = sys::GhosttySelectionBuffer {
            ptr: std::ptr::null_mut(),
            cap: 0,
            len: 0,
        };
        // SAFETY: the size-query form is the documented way to learn the
        // match capacity.
        let r = unsafe {
            sys::ghostty_search_get(
                self.search,
                sys::GhosttySearchData_GHOSTTY_SEARCH_DATA_MATCHES,
                &mut buffer as *mut _ as *mut c_void,
            )
        };
        if r != sys::GhosttyResult_GHOSTTY_OUT_OF_SPACE || buffer.len == 0 {
            return (Vec::new(), false);
        }
        let needed = buffer.len;
        let mut matches = vec![
            sys::GhosttySelection {
                size: std::mem::size_of::<sys::GhosttySelection>(),
                start: unsafe { std::mem::zeroed() },
                end: unsafe { std::mem::zeroed() },
                rectangle: false,
            };
            needed
        ];
        buffer.ptr = matches.as_mut_ptr();
        buffer.cap = needed;
        buffer.len = 0;
        // SAFETY: the buffer now holds `needed` initialized sized structs.
        let r = unsafe {
            sys::ghostty_search_get(
                self.search,
                sys::GhosttySearchData_GHOSTTY_SEARCH_DATA_MATCHES,
                &mut buffer as *mut _ as *mut c_void,
            )
        };
        if r != sys::GhosttyResult_GHOSTTY_SUCCESS {
            return (Vec::new(), false);
        }
        let history = self.history();
        let mut out: Vec<SearchMatch> = Vec::new();
        for sel in &matches[..buffer.len.min(needed)] {
            let (Some(start), Some(end)) = (point_of(handle, history, &sel.start), point_of(handle, history, &sel.end))
            else {
                continue;
            };
            let (start, end) = if (start.line, start.col) <= (end.line, end.col) {
                (start, end)
            } else {
                (end, start)
            };
            out.push(SearchMatch { start, end });
        }
        // Newest-first from the engine; oldest-first is the seam's order.
        out.reverse();
        let capped = out.len() >= cap;
        out.truncate(cap);
        (out, capped)
    }

    fn cursor_cell_raw(&self) -> Option<(usize, usize)> {
        // The Phase 3 contract: derived from the active-area cursor plus
        // the viewport offset — not the render state's cursor, which goes
        // silent once the cursor leaves the viewport. A cursor parked on a
        // wide tail reports the lead cell that owns the glyph.
        let cursor = self.term.cursor();
        let row = i32::from(cursor.y) + self.display_offset() as i32;
        if row < 0 {
            return None;
        }
        let mut col = usize::from(cursor.x);
        if let Some(r) = grid_ref(self.term.handle(), self.history(), GridPoint { line: cursor.y as i32, col: cursor.x })
            && glyph_of(&r).wide == CellWidth::SpacerTail
            && col > 0
        {
            col -= 1;
        }
        Some((row as usize, col))
    }

    fn frame_raw(&self, frame: &mut Frame, want_sliver: bool) {
        crate::ghostty_frame::fill(self, frame, want_sliver);
    }
}

// SAFETY: the engine owns only C handles the library documents as free of
// thread-affine state (see `Terminal`'s own `Send` note) plus plain Rust
// data; it is moved between threads behind the seam's mutex and never
// shared by reference.
unsafe impl Send for Engine {}

// ---- SessionTerm ------------------------------------------------------------

impl SessionTerm for Engine {
    fn cursor_style_raw(&self) -> CursorStyle {
        // DECSCUSR moves the style, and the render state reports what to
        // draw — one update, one read, and program restyles are seen the
        // same as the seam's own.
        crate::ghostty_frame::cursor_style_of(&self.render, self.term.handle())
    }

    fn set_cursor_style_raw(&mut self, style: Option<CursorStyle>) {
        // Spoken to the engine as DECSCUSR — the one language it takes for
        // this. `None` is the `0 q` reset, which restores the configured
        // default (see `apply_options`).
        let code = match style {
            None => 0,
            Some(style) => match (style.shape, style.blinking) {
                (CursorShape::Beam, true) => 5,
                (CursorShape::Beam, false) => 6,
                (CursorShape::Underline, true) => 3,
                (CursorShape::Underline, false) => 4,
                // Hollow is a renderer's unfocused block; as a DECSCUSR it
                // spells plain block.
                (CursorShape::Block | CursorShape::HollowBlock, true) => 1,
                (CursorShape::Block | CursorShape::HollowBlock, false) => 2,
            },
        };
        self.speak(format!("\x1b[{code} q").as_bytes());
    }

    fn clear_row_wrap_raw(&mut self, line: i32) {
        // No C call clears a row's soft-wrap flag, and a printed cell does
        // not reset it (which is why the engine being replaced needed a
        // direct grid op). What does clear it is an erase through the row's
        // end — `CSI 0 K` from the last cell — but that erase deletes
        // whatever the last cell holds. So the op acts only when the last
        // cell is blank (the shell-padded row its one caller, the
        // prompt-reflow repair, always brings); a row with content in its
        // last column is left untouched, wrap flag kept, and logged — the
        // seam's `clear_row_wrap` doc spells this contract.
        let cols = self.columns_raw();
        let clamped = line.clamp(0, self.screen_lines_raw() as i32 - 1);
        let blank = grid_ref(
            self.term.handle(),
            self.history(),
            GridPoint { line: clamped, col: (cols - 1) as u16 },
        )
        .map(|r| glyph_of(&r))
        .is_some_and(|g| g.c == ' ' && g.wide == CellWidth::Narrow);
        if !blank {
            log::debug!(
                "clear_row_wrap left row {line} wrapped: its last cell holds content \
                 (no engine op clears the flag without erasing the cell)"
            );
            return;
        }
        self.speak(format!("\x1b7\x1b[{};{cols}H\x1b[0K\x1b8", clamped + 1).as_bytes());
    }

    fn set_cursor_raw(&mut self, point: GridPoint, pending_wrap: bool) {
        // CUP, clamped by the engine itself into the active area. The
        // pending-wrap half has no byte spelling (it is parser state, not
        // screen state), so the position is restored and the flag is not —
        // one printed cell of drift in the parked-cursor edge where the
        // caret sat exactly past the last column. Phase 4 parity item.
        let _ = pending_wrap;
        let line = point.line.clamp(0, self.screen_lines_raw() as i32 - 1) + 1;
        let col = usize::from(point.col).min(self.columns_raw().saturating_sub(1)) + 1;
        self.speak(format!("\x1b[{line};{col}H").as_bytes());
    }

    fn set_selection_raw(
        &mut self,
        kind: SelectionKind,
        from: GridPoint,
        from_side: Side,
        to: GridPoint,
        to_side: Side,
    ) {
        self.install_selection(kind, (from, from_side), (to, to_side));
        self.selection.kind = kind;
        self.selection.anchor = Some((from, from_side));
        self.selection.unresolved = false;
    }

    fn start_selection_raw(&mut self, kind: SelectionKind, point: GridPoint, side: Side) {
        // A degenerate span the engine can hold, marked unresolved so the
        // reads answer "set, but covering nothing" — the drag gate's
        // contract.
        self.install_selection(kind, (point, side), (point, side));
        self.selection.kind = kind;
        self.selection.anchor = Some((point, side));
        // Word and line gestures resolve at once (their first click
        // already names a word / a row); only a `Simple` drag starts
        // unresolved.
        self.selection.unresolved = matches!(kind, SelectionKind::Simple);
    }

    fn update_selection_raw(&mut self, point: GridPoint, side: Side) {
        let Some((anchor, anchor_side)) = self.selection.anchor else {
            return;
        };
        let kind = self.selection.kind;
        self.install_selection(kind, (anchor, anchor_side), (point, side));
        self.selection.unresolved = false;
    }

    fn clear_selection_raw(&mut self) {
        self.clear_engine_selection();
        self.selection = SelectionState::default();
    }

    fn scroll_to_raw(&mut self, point: GridPoint) {
        // The minimal scroll that puts `point` on screen: compute where the
        // viewport sits, and when the point is outside it, land it on the
        // nearest edge — the engine's own scroll-to-point behavior.
        let offset = self.display_offset() as i32;
        let lines = self.screen_lines_raw() as i32;
        let top = -offset;
        let bottom = lines - 1 - offset;
        if point.line > bottom {
            // Bring it in at the bottom edge.
            let row = self.history() as i64 + point.line as i64 - (lines as i64 - 1);
            self.term
                .scroll(crate::terminal::ScrollTarget::RowOffset(row.max(0) as usize));
        } else if point.line < top {
            let row = self.history() as i64 + point.line as i64;
            self.term
                .scroll(crate::terminal::ScrollTarget::RowOffset(row.max(0) as usize));
        }
    }

    fn scroll_raw(&mut self, scroll: Scroll) {
        use crate::terminal::ScrollTarget;
        // The model's sign: positive goes up into scrollback; the engine's
        // delta says up is negative.
        let target = match scroll {
            Scroll::Delta(rows) => ScrollTarget::DeltaRows(-rows as isize),
            Scroll::PageUp => ScrollTarget::DeltaRows(-(self.screen_lines_raw() as isize)),
            Scroll::PageDown => ScrollTarget::DeltaRows(self.screen_lines_raw() as isize),
            Scroll::Top => ScrollTarget::Top,
            Scroll::Bottom => ScrollTarget::Bottom,
        };
        self.term.scroll(target);
    }

    fn clear_scrollback_raw(&mut self) {
        // `CSI 3 J` purges the history; a selection anchored in the rows
        // that went is dropped with them (the engine may instead clamp it
        // onto the viewport, which would select whatever text later moves
        // into those rows — not what the gesture meant).
        self.speak(b"\x1b[3J");
        self.clear_selection_raw();
    }

    fn resize_raw(&mut self, cols: usize, rows: usize) {
        let clamp = |n: usize| n.clamp(1, usize::from(u16::MAX)) as u16;
        let (cw, ch) = self.term.cell_px();
        // SAFETY-wrapped: the engine wrapper owns the error mapping; a
        // resize of a live terminal with in-range dimensions cannot fail.
        self.term
            .resize(clamp(cols), clamp(rows), cw, ch)
            .expect("resize with valid dimensions succeeds");
    }

    fn clear_below_raw(&mut self) {
        self.speak(b"\x1b[0J");
    }

    fn linefeed_raw(&mut self) {
        self.speak(b"\n");
    }
}

// ---- The owning facade -------------------------------------------------------

/// Forwards effects to a listener during one `write_with` — the adapter
/// between the wrapper's `&mut dyn EventSink` and the seam's `&self`
/// listener protocol.
struct ListenerSink<'a, S: VtEventListener>(&'a S);

impl<S: VtEventListener> crate::event::EventSink for ListenerSink<'_, S> {
    fn event(&mut self, event: VtEvent) {
        self.0.send_event(event);
    }
}

/// A terminal that owns its Ghostty engine state: parser, grid, scrollback,
/// and the listener its effects go to. The same surface
/// [`crate::alacritty::VtTerm`] has always exposed — the GUI's reader thread
/// holds one behind the seam's fair mutex, and the CLI and gateway replay
/// bytes through one directly.
pub struct VtTerm<S: VtEventListener = ()> {
    engine: Engine,
    listener: S,
}

impl VtTerm<()> {
    /// A `cols`×`rows` terminal keeping up to `scrollback_lines` lines of
    /// history, effects dropped. A zero dimension is raised to 1, the one
    /// size the engine cannot hold.
    pub fn new(cols: u16, rows: u16, scrollback_lines: usize) -> Self {
        Self::with_cursor_default(CursorStyle::default(), cols, rows, scrollback_lines)
    }

    /// [`new`](Self::new), with the cursor style a DECSCUSR reset (`0 q`)
    /// falls back to.
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
        let engine = Engine::new(options, cols, rows)
            .expect("libghostty-vt terminals only fail to allocate");
        Self { engine, listener }
    }

    /// A borrowed session over this terminal.
    pub fn session(&mut self) -> crate::engine::VtTermSession<'_> {
        crate::engine::VtTermSession::new(&mut self.engine)
    }

    /// A borrowed read-only view over this terminal.
    pub fn view(&self) -> crate::engine::VtTermView<'_> {
        crate::engine::VtTermView::new(&self.engine)
    }

    /// Parse `bytes` into the engine: the reader loop's hot path, one call
    /// per read.
    #[inline]
    pub fn advance(&mut self, bytes: &[u8]) {
        let Self { engine, listener } = self;
        let Engine { term, ed2, ed2_buf, .. } = engine;
        // The ED2 rewriter needs the screen fact as the *engine* sees it,
        // read before this chunk parses.
        let primary = term.cursor().screen == Screen::Primary;
        let bytes = ed2.rewrite(bytes, ed2_buf, primary);
        term.write_with(bytes, &mut ListenerSink(listener));
    }

    /// While a synchronized update (DEC mode 2026) is held open, the
    /// deadline after which it is flushed — the reader parks its read on
    /// this.
    pub fn sync_timeout(&self) -> Option<Instant> {
        self.engine.term.sync_deadline()
    }

    /// Flush a synchronized update ahead of its deadline: end the mode the
    /// way a program would.
    pub fn stop_sync(&mut self) {
        self.engine.term.write(b"\x1b[?2026l");
    }

    /// Tell the pane to shut down.
    pub fn exit(&mut self) {
        self.listener.send_event(VtEvent::Exit);
    }

    /// Clear the kitty keyboard flag stacks for the program that just left,
    /// on **both** screens.
    ///
    /// The stacks are per-screen and no C call reaches them, so this speaks
    /// the protocol's own escapes under a plain buffer switch (DEC 47 —
    /// switch only, no clear, no cursor save): over to the alternate
    /// screen, pop everything and set no flags, back, and the same on the
    /// primary. A plain 47 switch leaves each screen's contents, cursor,
    /// and saved state alone (pinned by the tests below), so the round
    /// trip is invisible past the flags it cleared.
    pub fn reset_keyboard_modes(&mut self) {
        self.advance(
            b"\x1b[?47h\x1b[<4096u\x1b[=0;1u\x1b[?47l\x1b[<4096u\x1b[=0;1u",
        );
    }

    /// Reset the emulator to a fresh grid, title and all — what a relink
    /// does before the daemon replays the pane from scratch.
    pub fn reset_state(&mut self) {
        self.engine.term.reset();
    }

    /// Apply new options (the user reconfigured their terminal, or the
    /// theme moved).
    pub fn set_options(&mut self, options: VtOptions) {
        self.engine.options = options;
        self.engine.apply_options();
    }

    /// The cell pixel size size reports and image protocols answer — the
    /// GUI's display metrics, which change without the grid changing.
    pub fn set_cell_size(&mut self, width: u32, height: u32) {
        self.engine.term.set_cell_px(width, height);
    }

    /// Whether this engine answers color queries from configured options —
    /// true here: OSC 4/10/11/12 replies come from the theme colors in
    /// `VtOptions`, so a theme push is what moves them.
    pub fn configures_colors(&self) -> bool {
        true
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.engine.resize_raw(usize::from(cols), usize::from(rows));
    }

    /// Read output. True when it brought a full-screen program's exit: the
    /// alternate screen was active when the call started and is not now.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let was_alt = self.alt();
        self.advance(bytes);
        was_alt && !self.alt()
    }

    pub fn alt(&self) -> bool {
        self.engine.alt_screen()
    }

    /// The whole picture as owned data: history rows, screen rows, cursor,
    /// modes. See [`Snapshot`].
    pub fn snapshot_full(&self) -> Snapshot {
        self.engine.snapshot_full()
    }

    /// The whole grid — scrollback plus screen — as plain text, wrapped
    /// rows joined, by the same rules the capture path pinned.
    pub fn text_full(&self) -> String {
        self.engine.term.text_full()
    }
}

/// The search facade's question: this engine's matcher is plain and
/// case-folding (`search.h`), so regex and case-sensitive queries are
/// refused rather than silently loosened — the status reads as
/// [`SearchStatus::Unsupported`] until the matcher grows them (Phase 3b).
pub(crate) fn query_status(query: &SearchQuery) -> SearchStatus {
    if query.text.is_empty() {
        return SearchStatus::Idle;
    }
    // Plain case-insensitive is the engine's own needle; everything else is
    // the text matcher — which compiles the same pattern the matcher will,
    // so a pattern it rejects reads as invalid before any terminal is
    // touched.
    if query.regex || query.case_sensitive {
        let pattern = match query.regex {
            true => query.text.clone(),
            false => regex_escape(&query.text),
        };
        return match regex::RegexBuilder::new(&pattern)
            .case_insensitive(!query.case_sensitive)
            .build()
        {
            Ok(_) => SearchStatus::Pending,
            Err(_) => SearchStatus::InvalidPattern,
        };
    }
    SearchStatus::Pending
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Frame, Scroll, SearchQuery as Query, SearchStatus};

    /// The collector the alacritty backend's tests also use, so event-order
    /// pins read the same on both sides of the differential line.
    #[derive(Default, Clone)]
    struct Tap(std::sync::Arc<std::sync::Mutex<Vec<VtEvent>>>);

    impl VtEventListener for Tap {
        fn send_event(&self, event: VtEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn fed(cols: u16, rows: u16, bytes: &[u8]) -> VtTerm {
        let mut term = VtTerm::new(cols, rows, 100);
        term.feed(bytes);
        term
    }

    /// The Phase 2f-1 contract: a drag's first click sets a selection that
    /// resolves to no span, and the copy gate has to see it.
    #[test]
    fn a_selection_starts_updates_and_gates_on_being_set() {
        let mut term = fed(10, 3, b"one two\r\nthree");
        {
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
        }
    }

    /// A selection reports its kind and span; a lines selection keeps whole
    /// rows (the `Right` edge at column 0 reads as the previous row's end).
    #[test]
    fn a_selection_reports_kind_span_and_text() {
        let mut term = fed(10, 3, b"one two\r\nthree");
        {
            let mut session = term.session();
            session.set_selection(
                SelectionKind::Simple,
                GridPoint { line: 0, col: 4 },
                Side::Left,
                GridPoint { line: 1, col: 4 },
                Side::Right,
            );
            let selection = session.selection().expect("a selection");
            assert_eq!(selection.kind, SelectionKind::Simple);
            assert_eq!(selection.span.start, GridPoint { line: 0, col: 4 });
            assert_eq!(selection.span.end, GridPoint { line: 1, col: 4 });

            session.set_selection(
                SelectionKind::Lines,
                GridPoint { line: 0, col: 0 },
                Side::Left,
                GridPoint { line: 1, col: 0 },
                Side::Right,
            );
            assert_eq!(
                session.selection_to_string().as_deref(),
                Some("one two\nthree\n"),
                "whole rows, trailing newline"
            );
        }
    }

    /// The 2f-2 contract: `cursor_cell` keeps reporting while the viewport
    /// is scrolled away from the cursor, derived from the active cursor
    /// plus the viewport offset — the render state's own cursor goes silent
    /// there and cannot back this read.
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
    }

    /// The 2d contract: a cursor parked on a wide tail reports the lead
    /// cell that owns the glyph — in `cursor_cell` and in the frame.
    #[test]
    fn a_cursor_on_a_wide_tail_reports_the_lead_cell() {
        let mut term = fed(10, 3, "你 x".as_bytes());
        term.session()
            .set_cursor(GridPoint { line: 0, col: 1 }, false);
        assert_eq!(term.view().cursor_cell(), Some((0, 0)));
        let mut frame = Frame::default();
        term.view().frame(&mut frame, false);
        assert_eq!(frame.cursor.expect("the cursor").x, 0, "the lead cell");
    }

    /// The frame: viewport rows with unresolved colors and style flags,
    /// wide pairs with their spacers, cursor visibility, alt screen, and
    /// the sliver row above the viewport.
    #[test]
    fn a_frame_reads_the_viewport_rows_and_their_style() {
        let term = fed(
            10,
            3,
            b"\x1b[1;3;4:3;38;2;1;2;3;48;5;9mx\x1b[0m y\x1b[2;9mz\x1b[0m",
        );
        let mut frame = Frame::default();
        term.view().frame(&mut frame, false);
        assert_eq!(frame.rows.len(), 3);
        assert_eq!(frame.rows[0].cells.len(), 10);
        let styled = &frame.rows[0].cells[0];
        assert_eq!(styled.c, 'x');
        assert!(styled.bold && styled.italic);
        assert_eq!(styled.underline, crate::model::Underline::Curly);
        assert_eq!(
            styled.fg,
            crate::model::StyleColor::Rgb(Rgb { r: 1, g: 2, b: 3 })
        );
        assert_eq!(styled.bg, crate::model::StyleColor::Palette(9));
        let faint = &frame.rows[0].cells[3];
        assert!(faint.dim && faint.strikethrough);
        assert_eq!(frame.cursor.map(|c| (c.x, c.y)), Some((4, 0)));
        assert_eq!(frame.display_offset, 0);
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
        assert!(!frame.cursor.expect("position survives").visible);

        term.feed(b"\x1b[?25h\r\nl2\r\nl3\r\nl4\r\nl5\r\n");
        term.session().scroll(Scroll::Delta(2));
        term.view().frame(&mut frame, false);
        assert!(frame.cursor.is_none());
        assert_eq!(frame.display_offset, 2);
        assert!(frame.history_size >= 2);
    }

    /// The sliver: the row above the viewport when scrollback has one —
    /// including at the live screen, where the last history row sits.
    #[test]
    fn a_frame_carries_the_sliver_row_when_scrolled() {
        let mut term = fed(10, 3, b"\x1b[31ml1\r\n\x1b[0ml2\r\nl3\r\nl4\r\nl5\r\n");
        term.session().scroll(Scroll::Delta(2));
        let mut frame = Frame::default();
        term.view().frame(&mut frame, true);
        let sliver = frame.sliver.as_ref().expect("a row above the viewport");
        assert_eq!(sliver.cells[0].c, 'l');
        assert_eq!(
            sliver.cells[0].fg,
            crate::model::StyleColor::Palette(1),
            "SGR crosses with it"
        );

        term.session().scroll(Scroll::Delta(-2));
        term.view().frame(&mut frame, true);
        assert!(
            frame.sliver.is_some(),
            "the row above the live screen is still scrollback's last"
        );

        let bare = fed(10, 2, b"solo");
        let mut frame = Frame::default();
        bare.view().frame(&mut frame, true);
        assert!(frame.sliver.is_none(), "no scrollback, nothing above");
    }

    /// The alt-screen flag and the mode bits the model knows.
    #[test]
    fn modes_and_alt_screen_read_off_the_engine() {
        let mut term = VtTerm::new(20, 4, 100);
        term.feed(b"before\r\n\x1b[?1049h\x1b[2J\x1b[1;1Halt");
        assert!(term.alt());
        let modes = term.view().modes();
        assert!(modes.contains(Modes::ALT_SCREEN));
        term.feed(b"\x1b[?1000h");
        assert!(term.view().modes().intersects(Modes::MOUSE_MODE));
        term.feed(b"\x1b[?1000l\x1b[?1002h");
        assert!(term.view().modes().intersects(Modes::MOUSE_MODE));
        term.feed(b"\x1b[?1002l");
        assert!(!term.view().modes().intersects(Modes::MOUSE_MODE));
        // The kitty keyboard protocol's flags — always spoken here (an
        // intended difference the harness allow-lists).
        term.feed(b"\x1b[>1u");
        assert!(term.view().modes().contains(Modes::KITTY_DISAMBIGUATE));
    }

    /// Geometry reaches into scrollback and scrolls by the model's targets.
    #[test]
    fn geometry_and_scrolling() {
        let mut term = fed(10, 3, b"l1\r\nl2\r\nl3\r\nl4\r\nl5\r\n");
        {
            let view = term.view();
            assert_eq!((view.columns(), view.screen_lines()), (10, 3));
            assert_eq!(view.topmost_line(), -3, "three rows scrolled off");
            assert_eq!(
                view.glyph(GridPoint { line: -3, col: 0 }).c,
                'l',
                "history is readable through the same coordinates"
            );
        }
        term.session().scroll(Scroll::Top);
        assert_eq!(term.view().viewport_offset(), 3);
        term.session().scroll(Scroll::Bottom);
        assert_eq!(term.view().viewport_offset(), 0);
    }

    /// Search: plain case-insensitive through `search.h`, regex and
    /// case-sensitive through the text matcher (decision 6) — all three
    /// served, oldest first, and a broken pattern reported as invalid
    /// rather than loosened.
    #[test]
    fn search_serves_plain_regex_and_case_queries() {
        let mut term = fed(20, 5, b"Hello World\r\nhello world\r\nWORLD wide");
        let status = |regex: bool, case: bool, text: &str| {
            query_status(&Query {
                text: text.to_string(),
                regex,
                case_sensitive: case,
            })
        };
        assert_eq!(status(false, false, ""), SearchStatus::Idle);
        assert_eq!(status(false, false, "world"), SearchStatus::Pending);
        assert_eq!(status(true, false, "wor.d"), SearchStatus::Pending);
        assert_eq!(status(false, true, "world"), SearchStatus::Pending);
        assert_eq!(status(true, false, "("), SearchStatus::InvalidPattern);

        let run = |term: &mut VtTerm, regex: bool, case: bool, text: &str| {
            let mut search = crate::VtSearch::new();
            search.set_query(Query {
                text: text.to_string(),
                regex,
                case_sensitive: case,
            });
            search.feed(&mut term.session());
            search.count()
        };
        assert_eq!(run(&mut term, false, false, "world"), 3);
        assert_eq!(run(&mut term, false, true, "world"), 1, "case-sensitive");
        assert_eq!(run(&mut term, true, false, "wor.d"), 3, "regex");
        assert_eq!(run(&mut term, true, true, "wor.d"), 1, "both at once");

        let mut search = crate::VtSearch::new();
        search.set_query(Query {
            text: "world".to_string(),
            regex: false,
            case_sensitive: false,
        });
        search.feed(&mut term.session());
        let lines: Vec<i32> = search.matches().iter().map(|m| m.start.line).collect();
        assert_eq!(lines, vec![0, 1, 2], "oldest first");
    }

    /// The text matcher's mapping: a match spanning a soft wrap lands as a
    /// two-row span, and a wide glyph's match is anchored at its lead cell.
    #[test]
    fn the_text_matcher_maps_wraps_and_wide_cells() {
        // 9 columns: "xxxxxxxxh" wraps, "it" continues on the next row.
        let mut term = fed(9, 4, b"xxxxxxxxhit y");
        let mut search = crate::VtSearch::new();
        search.set_query(Query {
            text: "hit".to_string(),
            regex: false,
            case_sensitive: true,
        });
        search.feed(&mut term.session());
        assert_eq!(search.count(), 1);
        let m = search.matches()[0];
        assert_eq!((m.start.line, m.start.col), (0, 8), "the lead cell");
        assert_eq!(m.end.line, 1, "the match spans the wrap");

        // A wide glyph in the match: the span's columns are its lead's.
        let mut term = fed(20, 3, "\u{4f60}\u{597d} here\r\nmore".as_bytes());
        let mut search = crate::VtSearch::new();
        search.set_query(Query {
            text: "\u{4f60}\u{597d}".to_string(),
            regex: false,
            case_sensitive: true,
        });
        search.feed(&mut term.session());
        assert_eq!(search.count(), 1);
        let m = search.matches()[0];
        assert_eq!((m.start.line, m.start.col, m.end.col), (0, 0, 2));
    }

    /// reset_keyboard_modes clears the kitty stacks on both screens and
    /// leaves the cursor, contents, and saved state where they were — the
    /// full shape of fork patch #4 (issue #1155), via a plain DEC 47
    /// buffer round trip.
    #[test]
    fn reset_keyboard_modes_clears_both_screens_and_nothing_else() {
        let kitty = Modes::KITTY_DISAMBIGUATE
            | Modes::KITTY_REPORT_ALL_KEYS
            | Modes::KITTY_REPORT_TEXT
            | Modes::KITTY_REPORT_EVENT_TYPES
            | Modes::KITTY_REPORT_ALTERNATE_KEYS;
        let mut term = VtTerm::new(20, 4, 10);
        // Flags pushed on the primary, more on the alternate, and the
        // program killed there — the reported pattern.
        term.feed(b"main text\r\nmore\r\n\x1b[>1u\x1b[?1049h\x1b[>7u alt\x1b[?1049l");
        let (cursor, text) = (term.view().cursor(), term.text_full());
        assert!(term.view().modes().intersects(kitty), "the push survived the exit");

        term.reset_keyboard_modes();

        assert_eq!(term.view().modes() & kitty, Modes::empty(), "primary cleared");
        assert_eq!(
            (term.view().cursor().x, term.view().cursor().y),
            (cursor.x, cursor.y),
            "the cursor never moved"
        );
        assert_eq!(term.text_full(), text, "the screen never changed");

        // The alternate screen's stack went with it.
        term.feed(b"\x1b[?1049h");
        assert_eq!(term.view().modes() & kitty, Modes::empty(), "alt cleared");
        term.feed(b"\x1b[?1049l");
    }

    /// DA1 answers the daemon's own detached reply — VT102, no features —
    /// so a program sees one terminal whichever side answered.
    #[test]
    fn device_attributes_answer_vt102_like_the_daemon() {
        let tap = Tap::default();
        let mut term = VtTerm::with_listener(VtOptions::default(), 20, 4, tap.clone());
        term.advance(b"\x1b[c");
        let events = tap.0.lock().unwrap().clone();
        assert!(
            events.iter().any(|e| matches!(e, VtEvent::PtyWrite(b) if b == b"\x1b[?6c")),
            "DA1 answers ESC[?6c, not {events:?}"
        );
    }

    /// Effects cross in order, as owned model events (the wrapper's pin,
    ///on the facade path the GUI drives).
    #[test]
    fn effects_arrive_through_the_facade() {
        let tap = Tap::default();
        let mut term = VtTerm::with_listener(VtOptions::default(), 40, 4, tap.clone());
        term.advance(b"\x1b]2;the title\x07\x07");
        let events = tap.0.lock().unwrap().clone();
        assert_eq!(events[0], VtEvent::Title("the title".into()));
        assert_eq!(events[1], VtEvent::Bell);
    }

    /// The steady-state frame: after a fill, rewriting one row copies that
    /// row and skips the rest — the dirty-tracking payoff the frame gate
    /// measures.
    #[test]
    fn a_frame_skips_clean_rows_it_can_prove_unchanged() {
        let mut term = fed(10, 3, b"one\r\ntwo\r\nthree");
        let mut frame = Frame::default();
        term.view().frame(&mut frame, false);
        let stamp = frame.generation;
        assert!(stamp > 0, "the fill stamped its generation");
        // A clean refill: same content, the rows must still read true.
        term.view().frame(&mut frame, false);
        assert_eq!(frame.rows[2].cells[0].c, 't');
        // New output dirties its row only; the rest keeps.
        term.feed(b"\x1b[2;1HX");
        term.view().frame(&mut frame, false);
        assert_eq!(frame.rows[1].cells[0].c, 'X', "the rewritten row crossed");
        assert_eq!(frame.rows[0].cells[0].c, 'o', "and the clean row kept");
    }

    /// A row wrap reads and clears: the erase form the clear takes (see
    /// `clear_row_wrap_raw`) erases the row's last cell, so the pin uses
    /// the padded-row shape the op's real caller always has — tail spaces
    /// from the shell, content before them.
    #[test]
    fn a_row_wrap_reads_and_clears() {
        let mut term = VtTerm::new(5, 3, 10);
        // "ab" then padding to the full width: a wrapped row whose tail is
        // blank, the prompt-reflow shape.
        term.feed(b"ab   cde");
        assert!(term.session().row_wraps(0), "the row soft-wrapped");
        term.session().clear_row_wrap(0);
        assert!(!term.session().row_wraps(0));
        assert!(term.text_full().starts_with("ab"), "the content kept");
    }

    /// The guard half of `clear_row_wrap`: a row whose last cell holds
    /// content is left fully untouched — the flag-clearing erase would
    /// delete that character, so the wrap stays instead.
    #[test]
    fn clearing_a_row_wrap_keeps_a_written_last_cell() {
        let mut term = VtTerm::new(5, 3, 10);
        // "abcde" fills the row and soft-wraps; the last cell holds 'e'.
        term.feed(b"abcdefg");
        assert!(term.session().row_wraps(0));
        term.session().clear_row_wrap(0);
        assert!(
            term.session().row_wraps(0),
            "a row with content in its last cell keeps its wrap"
        );
        let last = term.view().glyph(GridPoint { line: 0, col: 4 });
        assert_eq!(last.c, 'e', "the character survived the call");
    }

    /// Synchronized update: the BSU opens a deadline, the ESU (or the
    /// reader's stop_sync) closes it.
    #[test]
    fn a_held_sync_update_reports_its_deadline_until_flushed() {
        let mut term = VtTerm::new(10, 3, 10);
        assert_eq!(term.sync_timeout(), None);
        term.advance(b"\x1b[?2026habc");
        assert!(term.sync_timeout().is_some(), "the BSU opened a window");
        term.stop_sync();
        assert_eq!(term.sync_timeout(), None);
    }
}

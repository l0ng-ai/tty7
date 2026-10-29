//! The tracked anchor (Phase 4): an engine-owned reference to one cell
//! that follows it through scrolling, scrollback pruning, and reflow.
//!
//! [`crate::VtTermSession::track_point`] creates one where the engine can
//! (the Ghostty backend, through `grid_ref_tracked.h`); an engine without
//! tracking (alacritty, the default build) answers `None`, and its callers
//! keep the absolute-row anchors they have always used — the seam makes
//! the capability available, not mandatory.
//!
//! The one query [`TrackedAnchor`] answers is "which cell do I name now",
//! in SCREEN coordinates (rows from the top of scrollback, the space
//! [`GridPoint::line`](crate::model::GridPoint) counts from the bottom
//! of): a caller holding the history depth converts to its own rows. When
//! the row an anchor names is gone — pruned past the scrollback limit, or
//! its screen reset — the answer is `None`, which is exactly the fact a
//! drifting absolute-row anchor cannot express.
//!
//! Anchors are plain owned handles: they move between threads, they may
//! outlive their engine (they answer `None` after), and they free
//! themselves.

// Only the Ghostty backend's machinery names the model's point type; the
// default build's anchor is a bare `None`-shaped handle.
#[cfg(feature = "ghostty")]
use crate::model::GridPoint;

/// A tracked cell reference. `None`-shaped on builds whose engine has no
/// tracking; see the module docs.
pub struct TrackedAnchor {
    /// The engine's handle, on the Ghostty backend. Null-free by
    /// construction: creation either yields a live handle or `None`.
    #[cfg(feature = "ghostty")]
    raw: tty59_vt_sys::GhosttyTrackedGridRef,
}

// SAFETY (ghostty): an opaque engine handle with no thread affinity — the
// header's threading contract is caller-side serialization of *calls on
// the engine*, which the GUI's single engine lock already provides; an
// anchor's own queries are self-contained reads of its stored page data,
// and the header ships anchors across its own threads the same way.
// (No impl needed on the default build, where the struct has no fields.)
#[cfg(feature = "ghostty")]
unsafe impl Send for TrackedAnchor {}

// SAFETY: as `Send` — the handle is never mutated through a shared
// reference; resolution is a pure query, and freeing happens under the
// exclusive `Arc` that owns it.
#[cfg(feature = "ghostty")]
unsafe impl Sync for TrackedAnchor {}

impl TrackedAnchor {
    /// Wrap a live engine handle (the Ghostty backend's constructor).
    #[cfg(feature = "ghostty")]
    pub(crate) fn from_raw(raw: tty59_vt_sys::GhosttyTrackedGridRef) -> Self {
        Self { raw }
    }

    /// The cell this anchor names now, in screen coordinates — `y` counts
    /// from the top of the scrollable area (scrollback row 0), the space
    /// `history_size - y` converts to the model's `line`. `None` when the
    /// row is gone: pruned, reset, or its engine freed.
    #[cfg(feature = "ghostty")]
    pub fn screen_point(&self) -> Option<ScreenPoint> {
        use tty59_vt_sys as sys;
        let mut out = sys::GhosttyPointCoordinate { x: 0, y: 0 };
        // SAFETY: the handle was live at creation and stays owned; the
        // out-struct matches the documented type. A freed engine reports
        // NO_VALUE through this same call, which is the `None` below.
        let r = unsafe {
            sys::ghostty_tracked_grid_ref_point(
                self.raw,
                sys::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN,
                &mut out,
            )
        };
        (r == sys::GhosttyResult_GHOSTTY_SUCCESS).then_some(ScreenPoint {
            row: out.y as u64,
            col: out.x,
        })
    }

    /// The default build's answer: no tracking, no point.
    #[cfg(not(feature = "ghostty"))]
    pub fn screen_point(&self) -> Option<ScreenPoint> {
        None
    }
}

impl Drop for TrackedAnchor {
    #[cfg(feature = "ghostty")]
    fn drop(&mut self) {
        // SAFETY: owned handle, freed once; the engine may already be gone
        // (the handle outlives it by contract, and free still works).
        unsafe { tty59_vt_sys::ghostty_tracked_grid_ref_free(self.raw) };
    }

    #[cfg(not(feature = "ghostty"))]
    fn drop(&mut self) {}
}

/// A resolved anchor position in screen coordinates: the row from the top
/// of the scrollable area, the column from the grid's left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenPoint {
    pub row: u64,
    pub col: u16,
}

impl ScreenPoint {
    /// The model's `line` for this point at a given scrollback depth:
    /// `line` counts up from the active area's top (0 at the screen, −1
    /// per scrollback row), this counts down from the top of the
    /// scrollable area — so the topmost scrollback row (0) is line
    /// −`history_size`, and the active top is row `history_size`, line 0.
    pub fn line(self, history_size: u64) -> i32 {
        (self.row as i64 - history_size as i64) as i32
    }
}

/// Convert a model point to the C screen point the ghostty backend tracks
/// (shared by the backend's implementation; private to the crate).
#[cfg(feature = "ghostty")]
pub(crate) fn screen_point_of(history: usize, point: GridPoint) -> tty59_vt_sys::GhosttyPoint {
    tty59_vt_sys::GhosttyPoint {
        tag: tty59_vt_sys::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN,
        value: tty59_vt_sys::GhosttyPointValue {
            coordinate: tty59_vt_sys::GhosttyPointCoordinate {
                x: point.col,
                y: (history as i64 + point.line as i64) as u32,
            },
        },
    }
}

#[cfg(all(test, feature = "ghostty"))]
mod tests {
    use crate::model::GridPoint;
    use crate::VtTerm;

    /// An anchor follows its row through streaming, reflow-free, and says
    /// which cell it names at any later moment — the fact an absolute-row
    /// anchor only approximates.
    #[test]
    fn an_anchor_names_its_row_as_the_grid_moves() {
        let mut term = VtTerm::new(20, 4, 50);
        term.feed(b"target text\r\nfiller\r\n");
        let anchor = term
            .session()
            .track_point(GridPoint { line: 0, col: 0 })
            .expect("the ghostty backend tracks");
        // Nothing scrolled yet: the anchor names the active top, line 0.
        let before = anchor.screen_point().expect("still alive");
        assert_eq!(before.line(0), 0, "history 0, the active top");
        term.feed(b"more\r\nrows\r\npush\r\nit\r\ndown\r\n");
        let after = anchor.screen_point().expect("still alive");
        let history = term.view().history_size() as u64;
        let line = after.line(history);
        assert!(line < 0, "now in scrollback (line {line})");
        let mut frame = crate::model::Frame::default();
        term.view().frame(&mut frame, false);
        // Reading its text back proves the row it names is the row it meant.
        let text: String = term
            .view()
            .text(
                GridPoint { line, col: 0 },
                GridPoint { line, col: 11 },
            )
            .chars()
            .take(6)
            .collect();
        assert_eq!(text, "target", "the anchor still names its row");
    }

    /// When scrollback pruning takes the row, the anchor says so — `None`,
    /// not a silently wrong row. (A 5-line scrollback and enough output to
    /// carry the target out of it.)
    #[test]
    fn a_pruned_anchor_reports_nothing() {
        let mut term = VtTerm::new(20, 3, 5);
        term.feed(b"target\r\n");
        let anchor = term.session().track_point(GridPoint { line: 0, col: 0 }).unwrap();
        // The engine prunes at PAGE granularity — a small limit keeps
        // one whole page alive (measured: a 5-line cap held 19 rows), so
        // the row only goes once enough pages exist to spill. Feed past
        // page capacity: several thousand rows.
        term.feed("row\r\n".repeat(8_192).as_bytes());
        assert!(
            anchor.screen_point().is_none(),
            "the row was pruned; the anchor says nothing rather than lying"
        );
    }

    /// The default-build contract holds on this engine too: track_point
    /// hands back an anchor while the session lives (the alacritty
    /// backend's None shape is pinned in its own tests).
    #[test]
    fn anchors_survive_their_session_and_move() {
        let mut term = VtTerm::new(20, 3, 50);
        term.feed(b"text\r\n");
        let anchor = {
            let mut session = term.session();
            session.track_point(GridPoint { line: 0, col: 2 }).unwrap()
        };
        // The anchor outlives the session borrow and crosses a thread.
        let moved = std::thread::spawn(move || anchor.screen_point())
            .join()
            .unwrap();
        assert!(moved.is_some(), "still answers after moving");
    }
}

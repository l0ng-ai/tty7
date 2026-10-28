//! Glue between the seam's model types and the alacritty engine the GUI
//! still runs underneath. Every `alacritty_terminal` name that a migrated
//! file used to carry ends up here or in `remote.rs` (not yet migrated)
//! — nowhere else.

use alacritty_terminal::grid::Dimensions;

use crate::terminal::size::TermSize;

/// What `Term::new`/`resize` ask a size for. The impl lives here rather
/// than in `size.rs` so that file is pure model — a plain struct — while
/// the engine-facing trait stays with the engine glue (which is what
/// swapping engines deletes). Impls are crate-wide, so no call site moves.
impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

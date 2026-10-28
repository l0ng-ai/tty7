//! The pane's grid size, as the seam's plain model sees it: two counts.
//! The engine-facing `Dimensions` impl lives in `vt/alacritty.rs`, with
//! the rest of the engine glue.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermSize {
    pub cols: usize,
    pub rows: usize,
}

impl TermSize {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            cols: cols.max(1),
            rows: rows.max(1),
        }
    }
}

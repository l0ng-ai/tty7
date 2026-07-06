//! `TermSize`: the fixed grid dimensions handed to the VT emulator and the PTY.
//!
//! This used to live alongside an in-process PTY-backed `Terminal` here, but the
//! PTY now lives in the daemon (`daemon::pane`) and the GUI talks to it through
//! `terminal::remote::RemoteTerminal`. All that survives on the client side is
//! this size type, shared by the remote terminal and the view.

use alacritty_terminal::grid::Dimensions;

/// Fixed dimensions handed to `Term` / `Term::resize`.
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

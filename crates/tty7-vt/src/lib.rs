//! Safe wrapper around libghostty-vt, the renderer-free VT engine extracted
//! from Ghostty (docs/libghostty-migration.md).
//!
//! [`Terminal`] owns the C handle and is the only type that talks to
//! `tty7-vt-sys`. Everything a caller receives back — events, cells, styles,
//! text — is owned Rust data: no C pointer, borrowed string or sized-struct
//! ABI detail escapes a call, so the next Ghostty re-pin (one more ABI break
//! is expected before libghostty 1.0) is contained inside this crate.
//!
//! ## Threading
//!
//! libghostty-vt is single-threaded by contract: it keeps no thread-local
//! state and its callbacks run on the thread that called `vt_write`, which
//! must not re-enter it. `Terminal` therefore asserts `Send` but never
//! `Sync`: the design (the GUI's reader thread vs UI thread) shares one
//! terminal behind a mutex, and `&mut self` on every method keeps two
//! threads from ever holding it at once.
//!
//! ## Effects (callbacks)
//!
//! A bare terminal parses state changes only. Sequences with side effects —
//! BEL, OSC 0/2 titles, PTY replies — are dropped unless the embedder
//! registers callbacks. [`Terminal::write_with`] routes every registered
//! effect into an owned [`VtEvent`] on a caller-provided [`EventSink`],
//! wrapped in `catch_unwind` so a panicking sink can never unwind through
//! the C boundary into Zig (instant UB).

mod event;
mod snapshot;
mod terminal;
mod text;

#[cfg(test)]
mod tests;

pub use event::{EventLog, EventSink, ProgressState, VtEvent};
pub use snapshot::{Cell, CellWidth, Row, Semantic, Snapshot, StyleColor, Underline, Rgb};
pub use terminal::{Cursor, Error, Mode, ScrollTarget, Screen, Terminal};

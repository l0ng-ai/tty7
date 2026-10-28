//! Safe wrapper around libghostty-vt, Ghostty's renderer-free VT engine,
//! and home of the engine-neutral seam the migration moves consumers onto
//! (docs/libghostty-migration.md).
//!
//! ## The seam
//!
//! [`model`] holds tty59's own VT vocabulary — points, cells, snapshots,
//! modes, colors — as plain data with no engine behind it, and
//! [`VtTerm`]/[`VtTermSession`]/[`VtTermView`] are facades over whichever
//! engine is compiled in. The **default build is alacritty**: no FFI, no native
//! archive, exactly the code every build today already compiles. The
//! Ghostty engine arrives behind the `ghostty` cargo feature, which alone
//! pulls in `tty59-vt-sys` and the C library it links — so CI and
//! contributor builds stay Ghostty-free until the flip (Phase 3+).
//!
//! ## The Ghostty wrapper (feature `ghostty`)
//!
//! [`Terminal`] owns the C handle and is the only type that talks to
//! `tty59-vt-sys`. Everything a caller receives back — events, cells,
//! styles, text — is owned Rust data: no C pointer, borrowed string or
//! sized-struct ABI detail escapes a call, so the next Ghostty re-pin (one
//! more ABI break is expected before libghostty 1.0) is contained inside
//! this crate.
//!
//! ### Threading
//!
//! libghostty-vt is single-threaded by contract: it keeps no thread-local
//! state and its callbacks run on the thread that called `vt_write`, which
//! must not re-enter it. `Terminal` therefore asserts `Send` but never
//! `Sync`: the design (the GUI's reader thread vs UI thread) shares one
//! terminal behind a mutex, and `&mut self` on every method keeps two
//! threads from ever holding it at once.
//!
//! ### Effects (callbacks)
//!
//! A bare terminal parses state changes only. Sequences with side effects —
//! BEL, OSC 0/2 titles, PTY replies — are dropped unless the embedder
//! registers callbacks. [`Terminal::write_with`] routes every registered
//! effect into an owned [`VtEvent`] on a caller-provided [`EventSink`],
//! wrapped in `catch_unwind` so a panicking sink can never unwind through
//! the C boundary into Zig (instant UB).

pub mod model;

mod alacritty;
pub use alacritty::{VtTerm, VtTermSession, VtTermView};

mod search;
pub use search::VtSearch;

mod event;
pub use event::{ClipboardType, EventLog, EventSink, ProgressState, VtEvent, VtEventListener};

/// The fair mutex one shared engine lives behind (see [`sync`]).
mod sync;
pub use sync::FairMutex;

#[cfg(feature = "ghostty")]
mod terminal;
#[cfg(feature = "ghostty")]
pub use terminal::{Error, Mode, ScrollTarget, Terminal};

// The two model types the crate root has always named, still nameable
// from the root by the same paths.
pub use model::{Cursor, Screen};

/// The narrowest width a consumer's resize reasoning distinguishes — the
/// floor `alacritty_terminal::term::MIN_COLUMNS` names. The engine being
/// replaced enforces it nowhere (neither its constructor nor its resize
/// clamps); consumers borrow it to ask "does this resize change the column
/// count" without rounding a 1-column terminal up to 2. The facade keeps
/// parity by flooring at 1, so this constant is a decision aid, not a
/// guarantee about the grid.
pub const MIN_COLUMNS: usize = 2;

/// The narrowest height, alongside [`MIN_COLUMNS`]: the floor
/// `alacritty_terminal::term::MIN_SCREEN_LINES` names, borrowed by the same
/// resize reasoning.
pub const MIN_SCREEN_LINES: usize = 1;

#[cfg(feature = "ghostty")]
mod snapshot;
#[cfg(feature = "ghostty")]
mod text;

#[cfg(all(test, feature = "ghostty"))]
mod tests;

// The reader-loop parse bench (Phase 2e's perf gate), beside the wrapper
// tests it shares a profile with.
#[cfg(test)]
mod reader_bench;

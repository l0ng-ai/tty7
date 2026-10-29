//! Safe wrapper around libghostty-vt, Ghostty's renderer-free VT engine,
//! and home of the engine-neutral seam consumers talk to
//! (docs/libghostty-migration.md).
//!
//! ## The seam
//!
//! [`model`] holds tty59's own VT vocabulary — points, cells, snapshots,
//! modes, colors — as plain data with no engine behind it, and
//! [`VtTerm`]/[`VtTermSession`]/[`VtTermView`] are facades over the one
//! engine, libghostty-vt. Ghostty has been the only engine since Phase 6b
//! deleted the alacritty backend it replaced; the differential harness
//! that proved parity through the migration froze its corpus into the
//! golden tests (tests/golden), which now guard the engine's behavior.
//!
//! ## The wrapper
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

/// The tracked anchor: an engine-owned cell reference that follows its
/// row through scrolling, pruning and reflow (Phase 4).
mod anchor;
pub use anchor::{ScreenPoint, TrackedAnchor};

/// The backend contract and the engine-neutral borrowing facades
/// ([`VtTermSession`], [`VtTermView`]). The traits are sealed — implemented
/// only by the backends in this crate — and `pub` solely so the facade
/// constructors can name them in a bound.
pub mod engine;
pub use engine::{VtTermSession, VtTermView};

mod ghostty;
/// The engine behind [`VtTerm`]: libghostty-vt.
use ghostty as backend;

/// The facade every consumer names — the libghostty-vt engine.
pub use ghostty::VtTerm;

mod search;
pub use search::VtSearch;

mod event;
pub use event::{ClipboardType, EventLog, EventSink, ProgressState, VtEvent, VtEventListener};

/// The fair mutex one shared engine lives behind (see [`sync`]).
mod sync;
pub use sync::FairMutex;

mod ghostty_ed2;
mod ghostty_frame;
mod terminal;
pub use terminal::{Error, Mode, ScrollTarget, Terminal};

mod snapshot;
mod text;

#[cfg(test)]
mod tests;

// The reader-loop parse bench (Phase 2e's perf gate), beside the wrapper
// tests it shares a profile with.
#[cfg(test)]
mod reader_bench;

/// How many grid cells a codepoint occupies — **the engine's own table**,
/// the one its parser budgets columns with, for consumers that lay out
/// text the grid is about to receive (the input bar) and must not drift
/// from it. Per *codepoint*: cluster-level rules (a VS16 emoji reserving
/// two columns under mode 2027) are the grid's to apply — see
/// `grapheme_width` when a consumer needs cluster measurement.
pub fn codepoint_width(c: char) -> usize {
    // SAFETY: a plain u32 in, u8 out — no handles, no lifetime.
    unsafe { tty59_vt_sys::ghostty_unicode_codepoint_width(c as u32) as usize }
}

/// Cluster-level width — the grid's own grapheme rules (mode 2027's
/// segmentation): a VS16 presentation sequence is two columns, combining
/// marks add none. For consumers measuring strings the way the grid will
/// lay them out (the input bar's emoji expectations).
pub fn grapheme_width(cps: &[char]) -> usize {
    let mut units: Vec<u32> = cps.iter().map(|&c| u32::from(c)).collect();
    let mut width: u8 = 0;
    // SAFETY: `units` is a valid read buffer of `len` codepoints and
    // `width` a valid u8 out — the documented shapes.
    unsafe {
        tty59_vt_sys::ghostty_unicode_grapheme_width(units.as_mut_ptr(), units.len(), &mut width)
    };
    usize::from(width)
}

/// The narrowest width a consumer's resize reasoning distinguishes. The
/// replaced engine enforced it in its constructor and resize; this engine
/// floors at 1 by the same reasoning (a 1-column terminal is a valid
/// grid), so this constant is a decision aid about history, not a
/// guarantee about the grid.
pub const MIN_COLUMNS: usize = 2;

/// The narrowest height, alongside [`MIN_COLUMNS`]: the floor the replaced
/// engine named, borrowed by the same resize reasoning.
pub const MIN_SCREEN_LINES: usize = 1;

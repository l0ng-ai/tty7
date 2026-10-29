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

/// The tracked anchor: an engine-owned cell reference that follows its
/// row through scrolling, pruning and reflow (Phase 4). Engine-optional —
/// an engine without tracking yields no anchors and callers keep
/// absolute rows.
mod anchor;
pub use anchor::{ScreenPoint, TrackedAnchor};

/// The backend contract and the engine-neutral borrowing facades
/// ([`VtTermSession`], [`VtTermView`]). The traits are sealed — implemented
/// only by the backends in this crate — and `pub` solely so the facade
/// constructors can name them in a bound.
pub mod engine;
pub use engine::{VtTermSession, VtTermView};

mod alacritty;
/// The engine behind [`VtTerm`]: the alacritty backend by default, the
/// Ghostty one behind the `ghostty` feature. The search facade asks it
/// which queries it can serve; tests and the differential harness name it
/// directly.
#[cfg(not(feature = "ghostty"))]
use alacritty as backend;
#[cfg(feature = "ghostty")]
use ghostty as backend;

/// The default build's engine: the alacritty backend, plain Rust, no FFI —
/// exactly the engine every consumer runs today.
#[cfg(not(feature = "ghostty"))]
pub use alacritty::VtTerm;
/// With `ghostty` on, the facade the GUI names is the Ghostty engine; the
/// alacritty one stays compiled beside it as [`AlacTerm`] for the
/// differential harness (the point of keeping both: the two cannot drift
/// silently while the migration runs).
#[cfg(feature = "ghostty")]
pub use alacritty::VtTerm as AlacTerm;
#[cfg(feature = "ghostty")]
pub use ghostty::VtTerm;

mod search;
pub use search::VtSearch;

mod event;
pub use event::{ClipboardType, EventLog, EventSink, ProgressState, VtEvent, VtEventListener};

/// The fair mutex one shared engine lives behind (see [`sync`]).
mod sync;
pub use sync::FairMutex;

#[cfg(feature = "ghostty")]
mod ghostty;
#[cfg(feature = "ghostty")]
mod ghostty_ed2;
#[cfg(feature = "ghostty")]
mod ghostty_frame;
#[cfg(feature = "ghostty")]
mod terminal;
#[cfg(feature = "ghostty")]
pub use terminal::{Error, Mode, ScrollTarget, Terminal};

// The two model types the crate root has always named, still nameable
// from the root by the same paths.
pub use model::{Cursor, Screen};

/// How many grid cells a codepoint occupies — **the width the compiled-in
/// engine scores with**, for consumers that lay out text the grid is about
/// to receive (the input bar) and must not drift from it: behind `ghostty`
/// this is libghostty-vt's own table (the one its parser budgets columns
/// with), otherwise the `unicode-width` table the alacritty engine scores
/// with. Per *codepoint*: cluster-level rules (a VS16 emoji reserving two
/// columns under mode 2027) are the grid's to apply — see
/// `grapheme_width` when a consumer needs cluster measurement.
/// Cluster-level width — the grid's own grapheme rules (mode 2027's
/// segmentation on the Ghostty backend): a VS16 presentation sequence is
/// two columns, combining marks add none. Only meaningful behind `ghostty`
/// (the cluster rules it mirrors are that engine's); the default build's
/// consumers measure strings with the `unicode-width` fork the alacritty
/// grid scores with.
#[cfg(feature = "ghostty")]
pub fn grapheme_width(cps: &[char]) -> usize {
    let mut units: Vec<u32> = cps.iter().map(|&c| u32::from(c)).collect();
    let mut width: u8 = 0;
    // SAFETY: `units` is a valid read buffer of `len` codepoints and
    // `width` a valid u8 out — the documented shapes.
    unsafe {
        tty59_vt_sys::ghostty_unicode_grapheme_width(
            units.as_mut_ptr(),
            units.len(),
            &mut width,
        );
    }
    usize::from(width)
}

pub fn codepoint_width(c: char) -> usize {
    #[cfg(feature = "ghostty")]
    {
        // SAFETY: a plain u32 in, u8 out — no handles, no lifetime.
        unsafe { tty59_vt_sys::ghostty_unicode_codepoint_width(c as u32) as usize }
    }
    #[cfg(not(feature = "ghostty"))]
    {
        unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
    }
}

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

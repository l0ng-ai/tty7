//! The `terminal` subsystem, split by concern:
//! - [`size`] — `TermSize`, the grid dimensions shared by the remote terminal
//!   and the view.
//! - [`remote`] — the daemon-backed `RemoteTerminal`: owns a socket + a local
//!   mirror emulator, fed bytes the daemon replays instead of owning a PTY.
//! - [`view`] — the GPUI view that hosts a terminal and renders the chrome.
//! - [`element`] — the custom element that paints the character grid.
//! - [`palette`] — the terminal color scheme.
//!
//! Shell integration (the rc files that emit OSC 7 / OSC 133) used to live here
//! but now sits in `daemon::shell_integration`, beside the PTY-owning `pane`
//! that is its only injector — which is what keeps `daemon` from depending back
//! on `terminal`.
//!
//! `TermSize` / `RemoteTerminal` are re-exported here so the rest of the crate
//! can refer to `terminal::RemoteTerminal` without reaching into submodules.

mod cmd_editor;
mod completion;
pub mod element;
pub mod fps;
mod highlight;
mod history;
mod hold;
pub mod input;
pub mod palette;
mod remote;
mod reverse_search;
pub mod search;
mod signature;
mod size;
mod typeahead;
pub mod view;

pub use remote::RemoteTerminal;
pub use size::TermSize;

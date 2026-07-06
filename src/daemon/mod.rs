//! Persistent terminal daemon: keeps PTYs + their child processes alive across
//! GUI restarts (tmux-style detach/reattach), with the GUI acting as a thin
//! client over a Unix-domain socket.
//!
//! Layout:
//! - [`protocol`] — the framed wire messages shared by client and daemon.
//! - [`transport`] — the cross-platform local stream the protocol rides on
//!   (Unix-domain socket on Unix, loopback TCP on Windows).
//! - `pane` (daemon side) — owns one PTY/child, a replay ring, and fan-out.
//! - `server` (daemon side) — the listener, pane registry, `--daemon`
//!   entry point.
//! - `spawn` — endpoint resolution + auto-launching the daemon from the GUI.
//! - [`shell_integration`] — builds the throwaway `ZDOTDIR` (plus the bash/fish
//!   equivalents) whose rc files emit OSC 7 / OSC 133. Lives here because the
//!   PTY-owning `pane` is the sole injector; keeping it beside its only caller
//!   is what lets `daemon` avoid depending back on `terminal`.
//!
//! The client-side terminal that talks this protocol lives in
//! `terminal::remote::RemoteTerminal`, exposing the same surface as the old
//! in-process `Terminal` so the view layer is largely unchanged.

pub mod pane;
pub mod protocol;
pub mod server;
pub mod spawn;
pub mod transport;

// `pub(crate)` rather than private so a future non-daemon spawn path could reuse
// the exact same rc-file setup; today `pane` is the only caller.
pub(crate) mod shell_integration;

// Windows process-table helpers (foreground-command title + descendant teardown).
// Windows-only: the Unix path gets the same information from the pty's foreground
// process group and signals.
#[cfg(windows)]
pub(crate) mod winproc;

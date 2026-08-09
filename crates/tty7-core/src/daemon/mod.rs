pub mod control;
pub mod duplex;
pub mod install;
pub mod pane;
pub mod pidfile;
pub mod procinfo;
pub mod protocol;
pub(crate) mod remote;
pub mod remote_link;
pub mod router;
pub mod server;
pub mod singleton;
pub mod spawn;
pub mod ssh;
pub mod transport;

pub(crate) const DETECTED_SHELL_ENV: &str = "TTY7_DETECTED_SHELL";

pub(crate) mod shell_integration;

#[cfg(windows)]
pub(crate) mod winproc;

/// Windows-only like the updater flow that holds it: the macOS updater swaps
/// the bundle by rename and never stops the daemon, so it has no window in
/// which a fresh daemon could relock anything.
#[cfg(windows)]
pub mod update_guard;

/// The Windows environment refresh new panes get (#333). Only the registry
/// reader and the spawn wiring are Windows-only; the merge itself is a pure
/// function, so `cfg(test)` keeps the module compiling everywhere and its
/// semantics tested on every platform's CI runner rather than only on Windows.
#[cfg(any(windows, test))]
pub(crate) mod windows_env;

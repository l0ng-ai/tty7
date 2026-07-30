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
pub mod spawn;
pub mod ssh;
pub mod transport;

pub(crate) const DETECTED_SHELL_ENV: &str = "TTY7_DETECTED_SHELL";

pub(crate) mod shell_integration;

#[cfg(windows)]
pub(crate) mod winproc;

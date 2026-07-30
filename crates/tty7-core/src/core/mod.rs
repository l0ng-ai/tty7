//! Domain core: the configuration model, session persistence, the streaming
//! OSC tokenizer shared by the daemon- and client-side output scanners, and the
//! shell / agent / git knowledge the daemon and the GUI have to share.
//!
//! These modules are framework-light and depend on neither `ui` nor `terminal`
//! — the dependency arrow always points *inward* to here. That is what let them
//! lift out of the GUI binary into this crate without untangling view code.
//!
//! The GUI crate re-exports this module as `crate::core`, adding its own
//! gpui-facing modules (`actions`, `update`, …) and thin gpui layers over
//! `config`, `session` and `window_state`, so call sites there are unchanged.

pub mod agent_hooks;
pub mod cli_agent;
pub mod config;
pub mod crash;
pub mod git;
pub mod gitignore;
pub mod logfile;
pub mod machine;
// SSH connection-manager data layer (WS1). Its public API is consumed by the
// daemon-session, auth, forwarding, and UI workstreams, which land separately —
// so parts of it read as dead code until those merge.
#[allow(dead_code)]
pub mod keychain;
pub mod osc;
pub mod proc;
pub mod session;
pub mod shells;
#[allow(dead_code)]
pub mod ssh_profile;
pub mod threads;
pub mod window_state;
pub mod worktree;

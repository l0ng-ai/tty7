//! tty7's framework-free core.
//!
//! Everything that has to run on a machine with no display lives here: the
//! wire protocol, the session daemon (PTY ownership, replay rings, fan-out),
//! the native SSH engine, and the parts of the domain model — config, session
//! layout, shell/agent knowledge, git — that the GUI and the headless
//! `tty7-server` must agree on byte for byte.
//!
//! **This crate must never depend on gpui.** That is the invariant the split
//! exists to enforce;
//! `cargo tree -p tty7-core | grep gpui` must stay empty. Where a type genuinely
//! needs a gpui shape — `Config` as a `Global`, `WindowState` as a `Bounds`,
//! `FontFeatures` — the data lives here and the GUI crate adds the gpui-facing
//! layer on top.
//!
//! The module paths deliberately mirror what they were inside the old single
//! crate (`crate::core::config`, `crate::daemon::protocol`), and the GUI crate
//! re-exports them under the same names, so call sites read identically on
//! either side of the boundary.

pub mod core;
pub mod daemon;
pub mod host;

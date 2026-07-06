//! Domain core: the configuration model, session persistence, the action
//! vocabulary shared by the shell and the terminal view, and the streaming OSC
//! tokenizer shared by the daemon- and client-side output scanners.
//!
//! These modules are framework-light and depend on neither `ui` nor `terminal`,
//! so the dependency arrow always points *inward* to here. That keeps the door
//! open to lifting `core` into a standalone crate later without untangling view
//! code.

pub mod actions;
pub mod config;
pub mod osc;
pub mod session;
pub mod threads;

//! tty7's mobile gateway.
//!
//! Runs beside the tty7 server on a desktop and lets paired phones reach it
//! over iroh: dialed by public key, hole-punched where the network allows,
//! relayed where it does not, end-to-end encrypted either way. See
//! `tty7-mobile-proto` for what travels over the connection and why the phone
//! never speaks the daemon's own protocols.

pub mod daemon;
mod mirror;
pub mod poller;
pub mod route;
pub mod serve;
pub mod service;
pub mod state;
pub mod tree;

//! The framework-free half of tty7: wire protocol, session daemon, PTY, the
//! native SSH engine, and the domain model the headless `tty7-server` shares
//! with the GUI.

// A `pub` item here documenting how it relates to a private one — that
// `file_open_mode` is what `sanitize` fills in, that `restart_daemon` sends
// `TERMINATE_RUNNING_COMMAND` — is the useful half of these doc comments, and
// following the link is how a reader checks the claim. The lint exists to stop
// a *published* crate shipping docs whose links dead-end for anyone outside it;
// this crate is `publish = false`, and the only docs anyone builds for it come
// from `--document-private-items`, where every one of these targets is present.
#![allow(rustdoc::private_intra_doc_links)]

pub mod client;
pub mod core;
pub mod daemon;
pub mod host;

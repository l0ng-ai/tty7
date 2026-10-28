//! Raw FFI bindings to libghostty-vt, the renderer-free VT engine extracted
//! from Ghostty (pinned tag in `mise.toml`).
//!
//! `src/bindings.rs` is bindgen output checked in verbatim; regenerate it with
//! `scripts/regen-bindings.sh` after re-pinning Ghostty. Every type here is
//! exactly as unsafe as the C it mirrors — [`tty59_vt`] is the safe wrapper,
//! and nothing outside these two crates may touch this one.

// Bindings mirror C naming wholesale; silencing the style lints for the
// whole generated file is the point of a -sys crate.
#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

include!("bindings.rs");

//! Links the prebuilt static libghostty-vt archive.
//!
//! The archive is never fetched or built here: it comes from `$TTY59_GHOSTTY_DIR`,
//! a prefix holding `lib/` and `include/` that `mise run vt:build` produces
//! from the pinned Ghostty tag (see the root `mise.toml`). CI instead links
//! the per-target archive it publishes with SHA-256s, through the same
//! variable. Keeping the network out of `build.rs` is deliberate — a build
//! must not download native code, and contributors must not need Zig.

use std::{env, path::PathBuf};

fn main() {
    let prefix = env::var_os("TTY59_GHOSTTY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            panic!(
                "TTY59_GHOSTTY_DIR is not set. It points at the prefix holding the \
                 prebuilt libghostty-vt (`lib/` + `include/`). In this repo mise exports \
                 it; to build the library from the pinned Ghostty tag run \
                 `mise run vt:build` from the repo root."
            )
        });

    let lib_dir = prefix.join("lib");
    // Static link only: the shared libghostty exists for C embedders, but a
    // release binary must not grow a runtime .so dependency on an engine we
    // can ship in-tree (release packaging rejects one — migration doc,
    // decision 2).
    //
    // The name differs per platform because Ghostty's build names its static
    // artifact differently on Windows than the Unix `lib<name>.a` convention
    // expands to.
    let lib_name = if env::var("CARGO_CFG_WINDOWS").is_ok() {
        "ghostty-vt-static"
    } else {
        "ghostty-vt"
    };
    let archive = if env::var("CARGO_CFG_WINDOWS").is_ok() {
        lib_dir.join(format!("{lib_name}.lib"))
    } else {
        lib_dir.join(format!("lib{lib_name}.a"))
    };

    if !archive.is_file() {
        panic!(
            "libghostty-vt not found at {}. Run `mise run vt:build` from the repo \
             root (or fix TTY59_GHOSTTY_DIR) to build it from the pinned Ghostty tag.",
            archive.display()
        );
    }

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static={lib_name}");
    // The engine's scrollback compression and image placement map memory
    // directly; the archive references these POSIX libs without which the
    // final link fails on glibc targets. (Rust already links the rest of
    // libc's surface std needs.)
    if !env::var("CARGO_CFG_WINDOWS").is_ok() {
        println!("cargo:rustc-link-lib=m");
    }

    println!(
        "cargo:include={}",
        prefix.join("include").display()
    );
    // Rebuild when the archive is replaced by a `vt:build` of a different
    // Ghostty tag: the generated bindings describe the headers next to it.
    println!("cargo:rerun-if-changed={}", archive.display());
    // `links` metadata for downstream build scripts that want the headers.
    println!(
        "cargo:metadata=include={}",
        prefix.join("include").display()
    );
}

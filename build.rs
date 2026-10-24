fn main() {
    emit_material_icons();
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/favicon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/favicon.ico");
        if let Err(e) = res.compile() {
            println!("cargo:warning=failed to embed Windows icon: {e}");
        }
        stage_bundled_conpty();
    }
}

/// Emit the vendored Material icon pack as one table, keyed by file name, so
/// `Assets` can serve any of its paths without a match arm apiece.
///
/// `include_bytes!` wants a literal per file, and the pack is upstream's: a
/// re-sync replaces a thousand SVGs at once, which no hand-written table would
/// survive. Sorted here rather than at runtime so `Assets::load` can binary
/// search.
fn emit_material_icons() {
    use std::fmt::Write as _;

    let dir = std::path::Path::new("assets/icons/material");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut icons: Vec<(String, std::path::PathBuf)> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "svg"))
            .filter_map(|path| {
                let name = path.file_stem()?.to_str()?.to_owned();
                Some((name, path))
            })
            .collect(),
        Err(e) => {
            println!(
                "cargo:warning=no Material icon pack at {}: {e}",
                dir.display()
            );
            Vec::new()
        }
    };
    // By the name alone, byte-wise: that is the order `Assets::load` binary
    // searches in. Sorting the paths instead sorts them the way the OS compares
    // paths, which is not that order.
    icons.sort_by(|a, b| a.0.cmp(&b.0));

    let mut table = String::from("pub static MATERIAL_ICONS: &[(&str, &str, &[u8])] = &[\n");
    for (name, path) in &icons {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = name.as_str();
        // The name reaches a Rust string literal and a path inside `concat!`.
        // Every name in the pack is `[A-Za-z0-9._+-]+`, which needs no escaping in
        // either, so say so rather than silently skipping a surprising one.
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-')),
            "unexpected character in icon name {name:?}"
        );
        writeln!(
            table,
            "    ({name:?}, \"icons/material/{name}.svg\", \
             include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \
             \"/assets/icons/material/{name}.svg\"))),"
        )
        .expect("writing to a String cannot fail");
    }
    table.push_str("];\n");

    let out =
        std::path::Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("material_icons.rs");
    std::fs::write(&out, table).expect("write the Material icon table");
}

/// Put the bundled ConPTY beside cargo's output, the way the packaged app has
/// it beside `tty7-app.exe`.
///
/// `portable-pty` picks a sideloaded `conpty.dll` over the one in `kernel32`,
/// and finds it through the DLL search path — which starts at the directory of
/// the running executable, not the working directory. Without this a
/// development build silently runs on the in-box `conhost.exe` and loses the
/// `OSC 11` answer that packaged builds give (see `assets/windows/conpty`), so
/// the bug reappears only for people running from source.
///
/// Test executables live one directory deeper, so `deps/` gets a copy too.
#[cfg(windows)]
fn stage_bundled_conpty() {
    use std::path::{Path, PathBuf};

    println!("cargo:rerun-if-changed=assets/windows/conpty");

    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let vendored = match arch.as_str() {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        // 32-bit Windows is unreleased, so nothing is vendored for it and the
        // in-box conhost stays in charge.
        _ => return,
    };
    let source = PathBuf::from("assets/windows/conpty").join(vendored);

    // .../target/<triple>?/<profile>/build/<pkg>-<hash>/out
    let Some(target_dir) = std::env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .and_then(|out| out.ancestors().nth(3).map(Path::to_path_buf))
    else {
        return;
    };

    for name in ["conpty.dll", "OpenConsole.exe"] {
        let from = source.join(name);
        for directory in [target_dir.clone(), target_dir.join("deps")] {
            if !directory.is_dir() {
                continue;
            }
            let to = directory.join(name);
            // Watching the destination as well as the source is what makes the
            // staging self-healing: cargo treats a path that no longer exists
            // as changed, so a copy that gets cleaned out of the target
            // directory comes back on the next build instead of staying gone
            // behind a cached build script.
            println!("cargo:rerun-if-changed={}", to.display());
            // Re-copying would fail while a previously built tty7 is running,
            // and the pair only changes when it is deliberately updated.
            let same = std::fs::metadata(&to)
                .ok()
                .zip(std::fs::metadata(&from).ok())
                .is_some_and(|(a, b)| a.len() == b.len());
            if same {
                continue;
            }
            if let Err(e) = std::fs::copy(&from, &to) {
                println!(
                    "cargo:warning=could not stage {}: {e} — this build will use the in-box conhost",
                    to.display()
                );
            }
        }
    }
}

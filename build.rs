fn main() {
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
        // Every other Windows architecture is unreleased, so nothing is
        // vendored for it and the in-box conhost stays in charge.
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

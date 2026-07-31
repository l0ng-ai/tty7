//! Putting the bundled `tty7` CLI on PATH, without asking and without an entry
//! point to click.
//!
//! The GUI and the CLI ship as one artifact but are two binaries: the installer
//! lays `tty7` down beside `tty7-app` (inside `Contents/MacOS/` on macOS, in the
//! install directory elsewhere) and neither one is on PATH by virtue of being
//! there. Rather than a "Install shell command…" menu item that most people
//! never find, the GUI links it up itself on every launch — cheap enough to run
//! unconditionally, idempotent once it has succeeded.
//!
//! Two platform shapes, for reasons that are not symmetric:
//!
//! * **Unix** — symlink the CLI into a directory that is already on PATH. The
//!   alternative, putting our own directory on PATH, would mean editing the
//!   user's shell rc: a macOS GUI app inherits nothing from the login shell and
//!   cannot export into it. Writing to someone's `.zshrc` is a far bigger thing
//!   to do unprompted than dropping one symlink.
//! * **Windows** — the reverse. Symlinks need Developer Mode or elevation, and
//!   there is no conventional user-writable bin directory on PATH to link into.
//!   `HKCU\Environment` is the native answer and needs no privileges.
//!
//! Nothing here is fatal. Every failure path logs and returns; a user whose
//! system resists all of it still has a working GUI, just no `tty7` on PATH.

use std::path::{Path, PathBuf};

/// What a run of [`install`] did, for the log line and for tests.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Turned off in config.
    Disabled,
    /// No CLI beside the GUI — a hand-assembled tree, or a stripped bundle.
    NoBundledCli,
    /// A debug build: panes were wired up, the system was left untouched.
    DevBuild,
    /// Already reachable as `tty7`, pointing at this install.
    AlreadyInstalled(PathBuf),
    /// Freshly linked (or copied, under AppImage) into a directory on PATH.
    Installed(PathBuf),
    /// Installed somewhere the user's PATH does not currently cover.
    InstalledOffPath(PathBuf),
    /// Something is already called `tty7` on PATH and it is not ours to move.
    Occupied(PathBuf),
    /// Nowhere to write.
    Failed(String),
}

#[cfg(windows)]
const CLI_NAME: &str = "tty7.exe";
#[cfg(not(windows))]
const CLI_NAME: &str = "tty7";

/// Link the bundled CLI onto PATH, and make it reachable from panes right away.
///
/// Call this *before* the daemon is spawned: panes inherit their environment
/// from the daemon, which inherits it from this process, so the PATH entry
/// added here reaches every shell opened in this session — including the very
/// first one, and including the case where the on-disk half below fails
/// outright.
pub fn install() -> Outcome {
    let outcome = install_inner();
    match &outcome {
        Outcome::Disabled | Outcome::NoBundledCli | Outcome::DevBuild => {
            log::debug!("cli install skipped: {outcome:?}")
        }
        Outcome::AlreadyInstalled(p) => log::debug!("tty7 CLI already on PATH at {}", p.display()),
        Outcome::Installed(p) => log::info!("put the tty7 CLI on PATH at {}", p.display()),
        Outcome::InstalledOffPath(p) => log::warn!(
            "installed the tty7 CLI at {}, which is not on your PATH — add it to use `tty7` \
             outside a tty7 pane",
            p.display()
        ),
        Outcome::Occupied(p) => log::info!(
            "leaving the existing `tty7` at {} alone; the bundled CLI was not installed",
            p.display()
        ),
        Outcome::Failed(e) => log::warn!("could not put the tty7 CLI on PATH: {e}"),
    }
    outcome
}

fn install_inner() -> Outcome {
    if !crate::core::config::Config::load().install_cli_on_path {
        return Outcome::Disabled;
    }
    let Some(cli) = bundled_cli() else {
        return Outcome::NoBundledCli;
    };
    // Panes reach the CLI through the daemon's inherited environment even when
    // the on-disk half below is refused, so do this first and unconditionally.
    if let Some(dir) = cli.parent() {
        prepend_to_process_path(dir);
    }
    // A dev build gets the environment half and nothing else. `target/debug/`
    // holds a `tty7` too, so without this a `cargo run` would point the user's
    // real `tty7` at a debug binary — and the isolated instances the dev-verify
    // flow spins up would each rewrite the PATH of the machine they are meant
    // to be kept away from. Panes still get the build under test, which is the
    // half that development actually needs.
    if cfg!(debug_assertions) {
        return Outcome::DevBuild;
    }
    platform_install(&cli)
}

/// The CLI shipped alongside this GUI, if there is one.
///
/// Resolved relative to the running executable rather than searched for: the
/// point is to install *this build's* CLI, and a PATH search would find
/// whatever is already installed — including the symlink we made last launch,
/// which would then chase its own tail.
fn bundled_cli() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let cli = exe.parent()?.join(CLI_NAME);
    // `is_file` and not `exists`: on Unix the answer must be a real binary we
    // can exec, and a stale directory of that name should read as "absent".
    cli.is_file().then_some(cli)
}

/// Make the CLI reachable from this process's children (the daemon, and so
/// every pane) without waiting for the on-disk install to take effect.
///
/// Prepended rather than appended so it wins over a stale copy left on PATH by
/// an older install — inside a tty7 pane, `tty7` should mean the tty7 you are
/// sitting in.
fn prepend_to_process_path(dir: &Path) {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let already = std::env::split_paths(&current).any(|p| p == dir);
    if already {
        return;
    }
    let joined = std::iter::once(dir.to_path_buf())
        .chain(std::env::split_paths(&current))
        .collect::<Vec<_>>();
    match std::env::join_paths(joined) {
        // SAFETY: single-threaded startup — this runs from `main` before the
        // daemon is spawned and before gpui's executor exists, so there is no
        // concurrent reader of the environment.
        Ok(path) => unsafe { std::env::set_var("PATH", path) },
        Err(e) => log::warn!("could not extend PATH with {}: {e}", dir.display()),
    }
}

// ---- Unix ------------------------------------------------------------------

#[cfg(unix)]
fn platform_install(cli: &Path) -> Outcome {
    let path_dirs = path_dirs();
    let candidates = candidate_dirs(&path_dirs);

    let mut last_error = None;
    for dir in &candidates {
        match place(dir, cli) {
            Ok(Placement::Already(p)) => return Outcome::AlreadyInstalled(p),
            Ok(Placement::Wrote(p)) => {
                return if path_dirs.contains(dir) {
                    Outcome::Installed(p)
                } else {
                    Outcome::InstalledOffPath(p)
                };
            }
            Ok(Placement::Occupied(p)) => return Outcome::Occupied(p),
            Err(e) => last_error = Some(format!("{}: {e}", dir.display())),
        }
    }
    Outcome::Failed(last_error.unwrap_or_else(|| "no writable directory on PATH".into()))
}

/// The directories worth linking into, best first.
///
/// Deliberately a fixed list intersected with PATH rather than "the first
/// writable directory on PATH". Version-manager shim directories — pyenv,
/// rbenv, asdf, mise — sit at the *front* of PATH on a great many machines and
/// are writable, which makes them exactly what a first-writable scan picks. A
/// binary dropped there survives until that tool next rehashes and deletes
/// every file it did not put there. The failure is silent and arrives days
/// later, so the safe set is enumerated instead of discovered.
///
/// `~/.local/bin` is the fallback and is offered even when PATH does not list
/// it: an unreachable install the log names is a better outcome than no install
/// at all, and it is the one directory here we can always create.
#[cfg(unix)]
fn candidate_dirs(path_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let under_home = |rel: &str| home.as_ref().map(|h| h.join(rel));

    let preferred: Vec<PathBuf> = [
        Some(PathBuf::from("/opt/homebrew/bin")),
        Some(PathBuf::from("/usr/local/bin")),
        under_home(".local/bin"),
        under_home("bin"),
        under_home(".cargo/bin"),
    ]
    .into_iter()
    .flatten()
    .collect();

    let mut out: Vec<PathBuf> = preferred
        .iter()
        .filter(|d| path_dirs.contains(d))
        .cloned()
        .collect();
    if let Some(fallback) = under_home(".local/bin").filter(|f| !out.contains(f)) {
        out.push(fallback);
    }
    out
}

#[cfg(unix)]
enum Placement {
    Already(PathBuf),
    Wrote(PathBuf),
    Occupied(PathBuf),
}

/// AppImage mounts itself at a fresh `/tmp/.mount_XXXX` every run, so a symlink
/// into the bundle is dangling the moment the app exits. Copy there instead.
#[cfg(unix)]
fn running_from_appimage() -> bool {
    std::env::var_os("APPIMAGE").is_some()
}

#[cfg(unix)]
fn place(dir: &Path, cli: &Path) -> std::io::Result<Placement> {
    std::fs::create_dir_all(dir)?;
    let target = dir.join(CLI_NAME);

    match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let points_at = std::fs::read_link(&target)?;
            if points_at == cli {
                return Ok(Placement::Already(target));
            }
            // Replace only a link that is still aimed at something named
            // `tty7`. Anything else under this name was pointed somewhere
            // deliberate by its owner, and an auto-installer is not the thing
            // that gets to overrule that.
            if points_at.file_name() != Some(CLI_NAME.as_ref()) {
                return Ok(Placement::Occupied(target));
            }
        }
        // A real file: a `cargo install` build, a package manager's copy, or
        // the AppImage copy we made ourselves. Only the last is ours to touch.
        Ok(_) => {
            if !running_from_appimage() {
                return Ok(Placement::Occupied(target));
            }
            if same_size(&target, cli) {
                return Ok(Placement::Already(target));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    write_atomically(dir, &target, cli)?;
    Ok(Placement::Wrote(target))
}

/// Whether the installed copy already matches, by size alone.
///
/// Enough for the one case that asks: an AppImage upgrade, where a changed CLI
/// is a different build and a same-size rebuild of the identical source would
/// be a no-op anyway. Hashing megabytes on every launch to sharpen that is not
/// a trade worth making.
#[cfg(unix)]
fn same_size(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.len() == b.len(),
        _ => false,
    }
}

/// Write through a temporary name and rename over the target.
///
/// `std::os::unix::fs::symlink` fails outright if the destination exists, and
/// unlink-then-create leaves a window in which `tty7` resolves to nothing. The
/// rename is atomic, so a concurrent shell either sees the old entry or the new
/// one — never neither.
#[cfg(unix)]
fn write_atomically(dir: &Path, target: &Path, cli: &Path) -> std::io::Result<()> {
    // The temp name carries the pid so two tty7 instances starting together
    // cannot collide on it.
    let tmp = dir.join(format!(".{CLI_NAME}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);

    let result = if running_from_appimage() {
        std::fs::copy(cli, &tmp).and_then(|_| {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
        })
    } else {
        std::os::unix::fs::symlink(cli, &tmp)
    };
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(unix)]
fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

// ---- Windows ---------------------------------------------------------------

/// Append the CLI's directory to the *user's* PATH in the registry.
///
/// Reads `HKCU\Environment` rather than the process PATH on purpose. The
/// process value is the machine and user PATHs already merged, so writing it
/// back into the user hive would copy every system entry into HKCU — the
/// classic way installers permanently corrupt a PATH.
#[cfg(windows)]
fn platform_install(cli: &Path) -> Outcome {
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_EXPAND_SZ, REG_SZ, RegCloseKey,
        RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };

    let Some(dir) = cli.parent() else {
        return Outcome::Failed("the CLI has no parent directory".into());
    };

    let wide = |s: &str| {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };

    let subkey = wide("Environment");
    let value_name = wide("Path");
    let mut key: HKEY = std::ptr::null_mut();

    // SAFETY: all pointers below are to live locals, and every out-parameter is
    // initialised before the call. The key is closed on every return path.
    unsafe {
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_READ | KEY_WRITE,
            &mut key,
        ) != 0
        {
            return Outcome::Failed("could not open HKCU\\Environment".into());
        }

        // Read the current value. A missing `Path` is normal on a fresh
        // profile and means we are writing the first entry, not an error.
        let mut kind = 0u32;
        let mut bytes = 0u32;
        let existing = if RegQueryValueExW(
            key,
            value_name.as_ptr(),
            std::ptr::null_mut(),
            &mut kind,
            std::ptr::null_mut(),
            &mut bytes,
        ) == 0
        {
            let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
            if RegQueryValueExW(
                key,
                value_name.as_ptr(),
                std::ptr::null_mut(),
                &mut kind,
                buf.as_mut_ptr().cast(),
                &mut bytes,
            ) != 0
            {
                RegCloseKey(key);
                return Outcome::Failed("could not read the user PATH".into());
            }
            // Registry strings may or may not include their NUL.
            while buf.last() == Some(&0) {
                buf.pop();
            }
            std::ffi::OsString::from_wide(&buf)
                .to_string_lossy()
                .into_owned()
        } else {
            // Preserve REG_EXPAND_SZ if that is what was there; a fresh value
            // is a plain string.
            kind = REG_SZ;
            String::new()
        };

        let dir_str = dir.to_string_lossy();
        if existing.split(';').any(|e| {
            e.trim_end_matches('\\')
                .eq_ignore_ascii_case(dir_str.trim_end_matches('\\'))
        }) {
            RegCloseKey(key);
            return Outcome::AlreadyInstalled(cli.to_path_buf());
        }

        let updated = if existing.is_empty() {
            dir_str.to_string()
        } else {
            // Trailing `;` is legal but leaves an empty entry, which some
            // tools read as "the current directory" — trim before joining.
            format!("{};{dir_str}", existing.trim_end_matches(';'))
        };
        let updated_wide = wide(&updated);

        let kind = if kind == REG_EXPAND_SZ {
            REG_EXPAND_SZ
        } else {
            REG_SZ
        };
        let written = RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            kind,
            updated_wide.as_ptr().cast(),
            (updated_wide.len() * 2) as u32,
        );
        RegCloseKey(key);
        if written != 0 {
            return Outcome::Failed("could not write the user PATH".into());
        }

        // Without this, only processes started after the next sign-out pick the
        // change up: Explorer caches the environment it hands to what it
        // launches. The timeout keeps a hung top-level window from stalling
        // startup — the write already landed, so this is best-effort.
        let env = wide("Environment");
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            env.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5_000,
            std::ptr::null_mut(),
        );
    }

    Outcome::Installed(cli.to_path_buf())
}

#[cfg(not(any(unix, windows)))]
fn platform_install(_cli: &Path) -> Outcome {
    Outcome::Failed("unsupported platform".into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"#!/bin/sh\n").unwrap();
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tty7-cli-install-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_shim_directory_on_path_is_never_chosen() {
        // pyenv's shims are writable and come first on PATH; picking them would
        // get the binary deleted on the next rehash.
        let home = tmpdir("shims");
        let shims = home.join(".pyenv/shims");
        std::fs::create_dir_all(&shims).unwrap();
        let local = home.join(".local/bin");
        std::fs::create_dir_all(&local).unwrap();

        let path = vec![shims.clone(), local.clone()];
        let chosen = {
            // `candidate_dirs` reads $HOME for the user-relative entries.
            let _guard = EnvGuard::set("HOME", &home);
            candidate_dirs(&path)
        };
        assert!(!chosen.contains(&shims), "shim dir was offered: {chosen:?}");
        assert_eq!(chosen.first(), Some(&local));
    }

    #[test]
    fn an_unrelated_binary_named_tty7_is_left_alone() {
        let dir = tmpdir("occupied");
        let bin = tmpdir("occupied-src").join("tty7");
        touch(&bin);
        // Someone's own build, installed by hand.
        touch(&dir.join("tty7"));

        match place(&dir, &bin).unwrap() {
            Placement::Occupied(p) => assert_eq!(p, dir.join("tty7")),
            other => panic!(
                "clobbered a real binary: {:?}",
                matches!(other, Placement::Wrote(_))
            ),
        }
    }

    #[test]
    fn our_own_link_is_recognised_and_then_repointed_on_upgrade() {
        let dir = tmpdir("relink");
        let v1 = tmpdir("relink-v1").join("tty7");
        let v2 = tmpdir("relink-v2").join("tty7");
        touch(&v1);
        touch(&v2);

        assert!(matches!(place(&dir, &v1).unwrap(), Placement::Wrote(_)));
        // Second launch, same build: nothing to do.
        assert!(matches!(place(&dir, &v1).unwrap(), Placement::Already(_)));
        // Upgraded install: the link follows it rather than reporting a clash.
        assert!(matches!(place(&dir, &v2).unwrap(), Placement::Wrote(_)));
        assert_eq!(std::fs::read_link(dir.join("tty7")).unwrap(), v2);
    }

    #[test]
    fn a_link_aimed_somewhere_deliberate_is_not_hijacked() {
        let dir = tmpdir("deliberate");
        let bin = tmpdir("deliberate-src").join("tty7");
        touch(&bin);
        let elsewhere = tmpdir("deliberate-other").join("my-terminal");
        touch(&elsewhere);
        std::os::unix::fs::symlink(&elsewhere, dir.join("tty7")).unwrap();

        assert!(matches!(place(&dir, &bin).unwrap(), Placement::Occupied(_)));
    }

    #[test]
    fn the_process_path_gains_the_cli_directory_once() {
        let dir = tmpdir("procpath");
        let before = std::env::var("PATH").unwrap_or_default();
        prepend_to_process_path(&dir);
        let after = std::env::var("PATH").unwrap();
        assert!(after.starts_with(dir.to_str().unwrap()), "{after}");

        prepend_to_process_path(&dir);
        assert_eq!(std::env::var("PATH").unwrap(), after, "added twice");
        // SAFETY: single-threaded test.
        unsafe { std::env::set_var("PATH", before) };
    }

    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &Path) -> EnvGuard {
            let prev = std::env::var_os(key);
            // SAFETY: single-threaded test.
            unsafe { std::env::set_var(key, value) };
            EnvGuard { key, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: single-threaded test.
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }
}

#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod core;
mod daemon;
mod terminal;
mod ui;

use crate::core::config::Config;
use crate::ui::assets::Assets;
use crate::ui::keymap;
use gpui::*;

fn register_bundled_fonts(cx: &mut App) {
    use std::borrow::Cow;
    let fonts = vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/hack/Hack-Regular.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/hack/Hack-Bold.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/hack/Hack-Italic.ttf").as_slice()),
        Cow::Borrowed(include_bytes!("../assets/fonts/hack/Hack-BoldItalic.ttf").as_slice()),
    ];
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        log::warn!("failed to register bundled Hack fonts: {e}");
    }
}

fn spawn_config_watcher(cx: &mut App) {
    use notify::{RecursiveMode, Watcher};

    let Some(config_file) = crate::core::config::config_path("config.json") else {
        return;
    };
    let Some(dir) = crate::core::config::config_dir_path() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);

    const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

    let (tx, rx) = smol::channel::unbounded::<()>();
    let watched_file = config_file.clone();
    let handler = move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        let hit = event
            .paths
            .iter()
            .any(|p| p.file_name() == watched_file.file_name() || is_theme_file(p));
        if hit {
            let _ = tx.try_send(());
        }
    };

    let mut watcher = match notify::recommended_watcher(handler) {
        Ok(w) => w,
        Err(e) => {
            log::warn!("config hot-reload disabled: failed to create watcher: {e}");
            return;
        }
    };
    if let Err(e) = watcher.watch(&dir, RecursiveMode::Recursive) {
        log::warn!(
            "config hot-reload disabled: failed to watch {}: {e}",
            dir.display()
        );
        return;
    }
    Box::leak(Box::new(watcher));

    cx.spawn(async move |cx| {
        while rx.recv().await.is_ok() {
            cx.background_executor().timer(DEBOUNCE).await;
            while rx.try_recv().is_ok() {}

            cx.update(|cx| {
                let config = Config::load();
                crate::ui::i18n::set_locale(&config.gui_language);
                cx.set_global(config);
                crate::ui::presets::load_registry(cx);
                crate::ui::theme::apply_cursor_hide_mode(cx);
                crate::ui::theme::apply_theme(None, cx);
                // The menu bar is built once from the current locale, so editing
                // gui_language by hand has to rebuild it the same way the
                // in-app language picker does.
                crate::ui::theme::set_menus(cx);
                crate::ui::windows::WindowRegistry::refresh_locale(cx, None);
                cx.refresh_windows();
            });
        }
    })
    .detach();
}

fn is_theme_file(p: &std::path::Path) -> bool {
    p.parent().and_then(|d| d.file_name()) == Some(std::ffi::OsStr::new("themes"))
        && p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            e.eq_ignore_ascii_case("yaml")
                || e.eq_ignore_ascii_case("yml")
                || e.eq_ignore_ascii_case("itermcolors")
        })
}

fn strip_os_arg_prefix(arg: &std::ffi::OsStr, prefix: &str) -> Option<std::ffi::OsString> {
    let suffix = arg.as_encoded_bytes().strip_prefix(prefix.as_bytes())?;
    // SAFETY: `prefix` is ASCII and is removed only from the beginning of an
    // existing platform-encoded OsStr. ASCII bytes are self-synchronizing in
    // Windows WTF-8 and Unix byte strings, so the suffix keeps valid encoding.
    Some(unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(suffix.to_vec()) })
}

fn config_dir_from(
    mut args: impl Iterator<Item = std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    while let Some(arg) = args.next() {
        if let Some(path) = strip_os_arg_prefix(&arg, "--config-dir=") {
            return Some(path.into());
        }
        if arg == std::ffi::OsStr::new("--config-dir") {
            return args.next().map(Into::into);
        }
    }
    None
}

fn apply_config_dir_arg(args: &[std::ffi::OsString]) {
    if let Some(path) = config_dir_from(args.iter().cloned()) {
        crate::core::config::set_config_dir(path);
    }
}

fn open_path_from(
    mut args: impl Iterator<Item = std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    while let Some(arg) = args.next() {
        if let Some(path) = strip_os_arg_prefix(&arg, "--open-path=") {
            return Some(path.into());
        }
        if arg == std::ffi::OsStr::new("--open-path") {
            return args.next().map(Into::into);
        }
    }
    None
}

/// The directory an installer is about to replace, from
/// `--stop-daemon --update-install-dir <dir>`. Meaningful only next to
/// `--stop-daemon`; the caller checks that flag first.
#[cfg(windows)]
fn update_install_dir_from(
    mut args: impl Iterator<Item = std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    while let Some(arg) = args.next() {
        if arg == std::ffi::OsStr::new("--update-install-dir") {
            return args.next().map(Into::into);
        }
    }
    None
}

/// `Some(true)` to register the Explorer verbs, `Some(false)` to remove them.
fn explorer_menu_action_from(args: &[std::ffi::OsString]) -> Option<bool> {
    args.iter().find_map(|arg| match arg.as_os_str() {
        a if a == std::ffi::OsStr::new("--register-explorer-menu") => Some(true),
        a if a == std::ffi::OsStr::new("--unregister-explorer-menu") => Some(false),
        _ => None,
    })
}

/// Offers an explicit launch path to an already running local GUI.
///
/// The dispatcher is injected so the startup decision can be tested without
/// opening a real daemon connection. Only an explicit `Bool(true)` means the
/// path reached a GUI; every other response keeps the current app alive so it
/// can perform the normal first-window startup.
fn forward_open_path_with(
    open_path: Option<&std::path::Path>,
    dispatch: impl FnOnce(String) -> std::io::Result<tty7_core::daemon::control::ReplyOk>,
) -> bool {
    use tty7_core::daemon::control::ReplyOk;

    let Some(path) = open_path else {
        return false;
    };
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(current_dir) => current_dir.join(path),
            Err(error) => {
                log::debug!("could not resolve the relative GUI open path: {error}");
                return false;
            }
        }
    };

    let Some(wire_path) = path.to_str().map(str::to_owned) else {
        // Keep the native PathBuf in this process. Returning false continues
        // normal startup, which opens the path without crossing the protocol.
        log::debug!("the explicit GUI open path is not valid UTF-8; opening it locally");
        return false;
    };

    match dispatch(wire_path) {
        Ok(ReplyOk::Bool(true)) => true,
        Ok(ReplyOk::Bool(false)) => false,
        Ok(other) => {
            log::debug!("the daemon answered the early GuiOpen request with {other:?}");
            false
        }
        Err(error) => {
            log::debug!("the early GuiOpen request was not delivered: {error}");
            false
        }
    }
}

/// Uses a transient host-RPC connection instead of a GUI connection.
///
/// A GUI connection is long-lived and registers itself as the daemon's event
/// target. This short probe must never replace the actual running GUI while it
/// asks that GUI to open the requested folder.
fn forward_open_path(open_path: Option<&std::path::Path>) -> bool {
    use tty7_core::client::ControlClient;
    use tty7_core::daemon::control::{ControlHello, ControlRequest};

    forward_open_path_with(open_path, |path| {
        let hello = ControlHello::host_rpc(
            format!("tty7-app-open-{}", std::process::id()),
            "this computer",
        );
        let client = ControlClient::connect(&hello)?;
        let reply = client.request(ControlRequest::GuiOpen { path: Some(path) });
        client.close();
        reply
    })
}

#[cfg(unix)]
fn merge_paths(primary: &str, secondary: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    primary
        .split(':')
        .chain(secondary.split(':'))
        .filter(|p| !p.is_empty() && seen.insert(*p))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(unix)]
fn enrich_path_from_login_shell() {
    let shell = crate::core::shells::login_shell();
    let cmd = if std::path::Path::new(&shell).file_name() == Some("fish".as_ref()) {
        "string join ':' $PATH"
    } else {
        "echo $PATH"
    };
    let out = match std::process::Command::new(&shell)
        .args(["-l", "-c", cmd])
        .output()
    {
        Ok(out) if out.status.success() => out.stdout,
        Ok(out) => {
            log::warn!("login shell exited with {} while reading PATH", out.status);
            return;
        }
        Err(e) => {
            log::warn!("failed to spawn login shell {shell} for PATH: {e}");
            return;
        }
    };
    let login_path = String::from_utf8_lossy(&out).trim().to_string();
    if login_path.is_empty() {
        return;
    }
    let merged = merge_paths(&login_path, &std::env::var("PATH").unwrap_or_default());
    unsafe { std::env::set_var("PATH", merged) };
}

#[cfg(target_os = "macos")]
fn set_dock_icon_for_bare_binary() {
    use objc2::{AnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let bundled = std::env::current_exe().is_ok_and(|p| {
        p.components()
            .any(|c| c.as_os_str().to_string_lossy().ends_with(".app"))
    });
    if bundled {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    static ICON_PNG: &[u8] = include_bytes!("../assets/app-icon.png");
    let data = NSData::with_bytes(ICON_PNG);
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe {
            NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image));
        }
    }
}

fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    {
        if args.first().map(std::ffi::OsString::as_os_str)
            == Some(std::ffi::OsStr::new("agent-hook"))
        {
            if let (Some(agent), Some(event)) = (
                args.get(1).and_then(|arg| arg.to_str()),
                args.get(2).and_then(|arg| arg.to_str()),
            ) {
                crate::core::agent_hooks::run_agent_hook(agent, event);
            }
            return;
        }
    }

    apply_config_dir_arg(&args);

    let daemon = args
        .iter()
        .any(|arg| arg == std::ffi::OsStr::new("--daemon"));
    let role = if daemon { "daemon" } else { "gui" };
    crate::core::crash::install(role);
    crate::core::logfile::install(role);

    // The Windows installer owns the Explorer context menu: a task checkbox
    // runs these, and the uninstaller always runs the unregister half. Keeping
    // the registry shape in `explorer_context_menu` rather than in the .iss
    // means the installer and the running app can never disagree about it.
    // Handled after the log file is open, because a GUI-subsystem process has
    // no console to report a failure on and Inno does not surface exit codes:
    // the log is the only place the reason can survive.
    if let Some(register) = explorer_menu_action_from(&args) {
        let result = if register {
            crate::core::explorer_context_menu::register()
        } else {
            crate::core::explorer_context_menu::unregister()
        };
        if let Err(error) = result {
            log::error!("the Explorer context-menu update failed: {error}");
            std::process::exit(1);
        }
        return;
    }

    if daemon {
        if let Err(e) = crate::daemon::server::run_daemon() {
            log::error!("daemon exited with error: {e}");
        }
        return;
    }

    if args
        .iter()
        .any(|arg| arg == std::ffi::OsStr::new("--stop-daemon"))
    {
        // An installer about to replace `dir` says so, and gets more than a
        // stop: orphaned ConPTY hosts and anything else still running from
        // that directory are terminated, and the call does not return until
        // the images there are actually replaceable (or says why they are
        // not). Invoked by the Inno PrepareToInstall step and the updater.
        #[cfg(windows)]
        if let Some(dir) = update_install_dir_from(args.iter().cloned()) {
            // Held in the *parent's* name: this helper returns in seconds,
            // but the Setup (or uninstaller) that invoked it keeps replacing
            // files in `dir` until it exits — and a daemon spawned in that
            // window would relock them. The guard needs no clearing; it goes
            // stale the moment that parent is gone.
            tty7_core::daemon::update_guard::hold_for_parent();
            if let Err(error) = crate::daemon::spawn::stop_for_update(&dir) {
                log::error!(
                    "preparing {} for replacement failed: {error}",
                    dir.display()
                );
                std::process::exit(1);
            }
            return;
        }
        crate::daemon::spawn::stop();
        return;
    }

    #[cfg(unix)]
    enrich_path_from_login_shell();

    let open_path = open_path_from(args.into_iter());
    if forward_open_path(open_path.as_deref()) {
        return;
    }

    // A package the user asked to have applied at the next launch. Deliberately
    // here: after the forward above, so a second launch that is really a
    // request to an existing window never replaces the bundle out from under
    // it, and before everything below, so there is no daemon to strand and no
    // window to flash. On success the updater takes over and this process is
    // done; on failure it has already discarded the plan and we start normally.
    if crate::core::update::apply_pending_at_launch() {
        return;
    }

    let config = crate::core::config::Config::load();
    let gui_language = config.gui_language.clone();

    // After the PATH enrichment above, which is what makes the candidate scan
    // see the user's real PATH rather than the stub a Finder launch inherits —
    // and before the daemon below, which forks every pane and so must already
    // carry the CLI's directory in its environment.
    crate::core::cli_install::install(config.install_cli_on_path);

    // Give desktop toasts the tty7 icon and name instead of notify-rust's
    // PowerShell fallback. Best-effort; no-op off Windows.
    #[cfg(target_os = "windows")]
    crate::core::aumid::init();

    let restore_session = config.restore_session;
    let daemon_result = if restore_session {
        crate::daemon::spawn::ensure_running()
    } else {
        crate::daemon::spawn::restart()
    };
    if let Err(e) = daemon_result {
        log::error!("failed to ensure daemon is running: {e}");
    }

    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            register_bundled_fonts(cx);
            cx.activate(true);
            #[cfg(target_os = "macos")]
            set_dock_icon_for_bare_binary();
            crate::ui::i18n::set_locale(&gui_language);
            cx.set_global(Config::load());
            crate::ui::theme::refresh_system_appearance(cx);
            crate::core::session::WorkspaceStore::init(cx);
            crate::ui::windows::WindowRegistry::init(cx);
            crate::ui::presets::load_registry(cx);
            crate::ui::theme::apply_cursor_hide_mode(cx);
            spawn_config_watcher(cx);
            crate::core::update::spawn_check(cx);
            cx.background_executor()
                .spawn(async {
                    crate::core::agent_hooks::refresh_hooks_at_launch();
                })
                .detach();
            keymap::init(cx);
            crate::ui::local_link::LocalLink::install(cx);

            let reopen = open_path
                .is_none()
                .then(|| crate::core::session::WorkspaceStore::restore_one(cx))
                .flatten();
            crate::ui::windows::open_at(cx, reopen, open_path);
        });
}

#[cfg(all(test, unix))]
mod tests {
    use super::merge_paths;

    #[test]
    fn merge_paths_prefers_primary_dedupes_and_drops_empties() {
        assert_eq!(
            merge_paths("/opt/homebrew/bin:/usr/bin", "/usr/bin:/bin:"),
            "/opt/homebrew/bin:/usr/bin:/bin"
        );
        assert_eq!(
            merge_paths(
                "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin",
                "/usr/bin:/bin"
            ),
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
        );
        assert_eq!(merge_paths("", "/usr/bin"), "/usr/bin");
    }
}

#[cfg(test)]
mod argument_tests {
    use super::{
        config_dir_from, explorer_menu_action_from, forward_open_path_with, open_path_from,
    };
    use std::ffi::OsString;
    use std::path::PathBuf;
    use tty7_core::daemon::control::ReplyOk;

    /// The installer passes exactly one of these; every other launch — above
    /// all a plain `--open-path` from the very menu they register — must fall
    /// through to normal startup instead of rewriting the registry and exiting.
    #[test]
    fn explorer_menu_flags_are_distinguished_and_otherwise_absent() {
        assert_eq!(
            explorer_menu_action_from(&[OsString::from("--register-explorer-menu")]),
            Some(true)
        );
        assert_eq!(
            explorer_menu_action_from(&[OsString::from("--unregister-explorer-menu")]),
            Some(false)
        );
        assert_eq!(
            explorer_menu_action_from(&[OsString::from("--open-path"), OsString::from("/work")]),
            None
        );
        assert_eq!(explorer_menu_action_from(&[]), None);
    }

    #[test]
    fn open_path_accepts_separate_and_equals_forms() {
        let separate = open_path_from(
            ["--config-dir", "/cfg", "--open-path", "/work"]
                .into_iter()
                .map(OsString::from),
        );
        assert_eq!(separate, Some(PathBuf::from("/work")));

        let equals = open_path_from([OsString::from("--open-path=C:\\work")].into_iter());
        assert_eq!(equals, Some(PathBuf::from("C:\\work")));
    }

    #[test]
    fn config_dir_accepts_native_separate_and_equals_forms() {
        let separate = config_dir_from(
            [OsString::from("--config-dir"), OsString::from("C:\\cfg")].into_iter(),
        );
        assert_eq!(separate, Some(PathBuf::from("C:\\cfg")));

        let equals = config_dir_from([OsString::from("--config-dir=C:\\cfg")].into_iter());
        assert_eq!(equals, Some(PathBuf::from("C:\\cfg")));
    }

    #[cfg(windows)]
    #[test]
    fn open_path_preserves_unpaired_utf16_from_windows_arguments() {
        use std::os::windows::ffi::OsStringExt as _;

        let native_path =
            OsString::from_wide(&[b'C' as u16, b':' as u16, b'\\' as u16, 0xD800, b'x' as u16]);
        let parsed =
            open_path_from([OsString::from("--open-path"), native_path.clone()].into_iter());
        assert_eq!(parsed, Some(PathBuf::from(native_path.clone())));

        let mut equals_arg = OsString::from("--open-path=");
        equals_arg.push(&native_path);
        assert_eq!(
            open_path_from([equals_arg].into_iter()),
            Some(PathBuf::from(native_path))
        );
    }

    #[test]
    fn no_open_path_never_attempts_early_dispatch() {
        let delivered = forward_open_path_with(None, |_| {
            panic!("the dispatcher must not run without an explicit path")
        });

        assert!(!delivered);
    }

    #[test]
    fn early_dispatch_exits_only_after_a_gui_accepts_the_path() {
        let path = std::env::current_dir().unwrap().join("folder with spaces");
        let expected = path.to_str().unwrap().to_owned();
        let delivered = forward_open_path_with(Some(&path), |actual| {
            assert_eq!(actual, expected);
            Ok(ReplyOk::Bool(true))
        });

        assert!(delivered);
        assert!(!forward_open_path_with(Some(&path), |_| Ok(ReplyOk::Bool(
            false
        ))));
        assert!(!forward_open_path_with(Some(&path), |_| Ok(ReplyOk::Unit)));
        assert!(!forward_open_path_with(Some(&path), |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "daemon is not running",
            ))
        }));
    }

    #[test]
    fn early_dispatch_resolves_relative_paths_before_cross_process_delivery() {
        let relative = std::path::Path::new("workspace");
        let expected = std::env::current_dir()
            .unwrap()
            .join(relative)
            .to_str()
            .unwrap()
            .to_owned();

        assert!(forward_open_path_with(Some(relative), |actual| {
            assert_eq!(actual, expected);
            Ok(ReplyOk::Bool(true))
        }));
    }

    #[cfg(unix)]
    #[test]
    fn early_dispatch_keeps_non_utf8_paths_in_the_current_process() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let path = std::env::temp_dir().join(OsString::from_vec(b"tty7-\xff".to_vec()));
        let delivered = forward_open_path_with(Some(&path), |_| {
            panic!("a non-UTF-8 path must never be changed and sent over the string protocol")
        });

        assert!(!delivered);
    }
}

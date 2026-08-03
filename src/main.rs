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
                cx.set_global(Config::load());
                crate::ui::presets::load_registry(cx);
                crate::ui::theme::apply_cursor_hide_mode(cx);
                crate::ui::theme::apply_theme(None, cx);
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

fn apply_config_dir_arg() {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(path) = arg.strip_prefix("--config-dir=") {
            crate::core::config::set_config_dir(path.into());
            return;
        }
        if arg == "--config-dir" {
            if let Some(path) = args.next() {
                crate::core::config::set_config_dir(path.into());
            }
            return;
        }
    }
}

fn open_path_arg() -> Option<std::path::PathBuf> {
    open_path_from(std::env::args().skip(1))
}

fn open_path_from(mut args: impl Iterator<Item = String>) -> Option<std::path::PathBuf> {
    while let Some(arg) = args.next() {
        if let Some(path) = arg.strip_prefix("--open-path=") {
            return Some(path.into());
        }
        if arg == "--open-path" {
            return args.next().map(Into::into);
        }
    }
    None
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
    {
        let args: Vec<String> = std::env::args().skip(1).take(3).collect();
        if args.first().map(String::as_str) == Some("agent-hook") {
            if let [_, agent, event] = args.as_slice() {
                crate::core::agent_hooks::run_agent_hook(agent, event);
            }
            return;
        }
    }

    apply_config_dir_arg();

    let role = if std::env::args().any(|a| a == "--daemon") {
        "daemon"
    } else {
        "gui"
    };
    crate::core::crash::install(role);
    crate::core::logfile::install(role);

    if std::env::args().any(|a| a == "--daemon") {
        if let Err(e) = crate::daemon::server::run_daemon() {
            log::error!("daemon exited with error: {e}");
        }
        return;
    }

    if std::env::args().any(|a| a == "--stop-daemon") {
        crate::daemon::spawn::stop();
        return;
    }

    #[cfg(unix)]
    enrich_path_from_login_shell();

    let open_path = open_path_arg();
    if forward_open_path(open_path.as_deref()) {
        return;
    }
    let config = crate::core::config::Config::load();

    // After the PATH enrichment above, which is what makes the candidate scan
    // see the user's real PATH rather than the stub a Finder launch inherits —
    // and before the daemon below, which forks every pane and so must already
    // carry the CLI's directory in its environment.
    crate::core::cli_install::install(config.install_cli_on_path);

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
    use super::{forward_open_path_with, open_path_from};
    use std::path::PathBuf;
    use tty7_core::daemon::control::ReplyOk;

    #[test]
    fn open_path_accepts_separate_and_equals_forms() {
        let separate = open_path_from(
            ["--config-dir", "/cfg", "--open-path", "/work"]
                .into_iter()
                .map(str::to_string),
        );
        assert_eq!(separate, Some(PathBuf::from("/work")));

        let equals = open_path_from(["--open-path=C:\\work".to_string()].into_iter());
        assert_eq!(equals, Some(PathBuf::from("C:\\work")));
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

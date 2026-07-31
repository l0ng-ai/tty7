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
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
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

    // After the PATH enrichment above, which is what makes the candidate scan
    // see the user's real PATH rather than the stub a Finder launch inherits —
    // and before the daemon below, which forks every pane and so must already
    // carry the CLI's directory in its environment.
    crate::core::cli_install::install();

    let restore_session = crate::core::config::Config::load().restore_session;
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

            let reopen = crate::core::session::WorkspaceStore::restore_one(cx);
            crate::ui::windows::open(cx, reopen);
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

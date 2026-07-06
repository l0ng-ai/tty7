mod core;
mod daemon;
mod terminal;
mod ui;

use crate::core::config::Config;
use crate::ui::app::Tty7App;
use crate::ui::keymap;
use gpui::*;
use gpui_component::{ActiveTheme as _, Root, TitleBar};
use gpui_component_assets::Assets;

/// Register the bundled Hack monospace faces with gpui's text system so the
/// default `font_family` ("Hack") renders identically on every machine, with no
/// dependency on the user having the font installed (the app bundles its
/// own copy). The four faces cover regular / bold / italic / bold-italic.
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

/// Watch `config.json` and hot-reload the app when it changes on disk, so
/// hand-edits (or an external tool rewriting the file) take effect live — no
/// restart. We watch the config *directory*, not the file: editors and our own
/// [`Config::save`] replace `config.json` via a temp-file + rename (atomic
/// write), which severs any watch bound to the original inode. Watching the
/// parent and filtering to `config.json` events survives the swap.
///
/// The `notify` callback fires on a background OS thread, which can't touch GPUI
/// state. We bridge to the app (main) thread the same way the daemon reader does
/// (see `terminal::remote`): a `smol::channel` carries a bare "something changed"
/// ping, and a `cx.spawn` task on the foreground executor drains it and does the
/// reload with a real `&mut App`.
///
/// Scope note: this re-applies theme + colors live (via `apply_theme`, which
/// reads the freshly-loaded `Config` global). Font size / line height / font
/// family are cached in `Tty7App`'s fields and pushed into each `TerminalView`,
/// so a live change to *those* keys needs a hook in `ui::app` (owned elsewhere);
/// they still take effect for newly-opened tabs and on restart. Font *family*
/// changes need no font re-registration: `add_fonts` is only for bundled/custom
/// face files (we ship Hack, registered once at startup); any other family is a
/// system font gpui resolves by name at render time.
fn spawn_config_watcher(cx: &mut App) {
    use notify::{RecursiveMode, Watcher};

    // Resolve the file we care about and the directory we actually watch. If the
    // config dir doesn't resolve (no override/env/$HOME) there's nothing to do.
    let Some(config_file) = crate::core::config::config_path("config.json") else {
        return;
    };
    let Some(dir) = crate::core::config::config_dir_path() else {
        return;
    };
    // The dir may not exist yet on a first run; watching a missing path errors.
    // Create it so the watch attaches (harmless — the daemon/save would too).
    let _ = std::fs::create_dir_all(&dir);

    // Coalesce a save's burst of events (truncate → write → rename can fire
    // several times) into a single reload: on the first ping we wait out a short
    // quiet period, drain anything queued, then reload once.
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

    let (tx, rx) = smol::channel::unbounded::<()>();
    let watched_file = config_file.clone();
    let handler = move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        // Only react to events that touch our `config.json` (the dir may also see
        // `session.json`, `history`, and our own `.config.json.tmp.<pid>` scratch
        // file from atomic writes — ignore those).
        let hit = event
            .paths
            .iter()
            .any(|p| p.file_name() == watched_file.file_name());
        if hit {
            // try_send: a full channel just means a reload is already pending;
            // one ping is enough to trigger the (idempotent) reload.
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
    if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
        log::warn!(
            "config hot-reload disabled: failed to watch {}: {e}",
            dir.display()
        );
        return;
    }
    // The `RecommendedWatcher` owns the background watch thread; dropping it stops
    // watching. It has to live for the whole app, so we intentionally leak it
    // rather than thread a handle through app state (there's exactly one, for the
    // process lifetime, so a one-off leak is the simplest correct choice).
    Box::leak(Box::new(watcher));

    cx.spawn(async move |cx| {
        while rx.recv().await.is_ok() {
            // Debounce: let the save settle, then swallow the rest of the burst so
            // we reload exactly once.
            cx.background_executor().timer(DEBOUNCE).await;
            while rx.try_recv().is_ok() {}

            cx.update(|cx| {
                // `Config::load` clamps/validates and falls back to defaults on a
                // parse error (a half-written file mid-edit), so a bad reload can
                // never crash the renderer — worst case we momentarily show
                // defaults until the next (valid) save re-triggers this.
                cx.set_global(Config::load());
                crate::ui::theme::apply_cursor_hide_mode(cx);
                // Re-paint theme + colors from the new config. We have no window
                // handle in this global task, but `apply_theme` accepts `None`:
                // it still updates the `Theme`/palette globals (what actually
                // repaints); the only thing it skips is re-pinning the macOS
                // traffic lights, which self-corrects on the next resize/activate.
                crate::ui::theme::apply_theme(None, cx);
                // Schedule every window to redraw so the new palette shows at once.
                cx.refresh_windows();
            });
        }
        // Loop only ends if every `Sender` drops — but the sole sender lives in
        // the leaked watcher's handler, so in practice this runs for the app's
        // lifetime.
    })
    .detach();

    // Note on feedback loops: our own `Config::save` (theme toggle, font zoom)
    // rewrites `config.json` and will trip this watcher. That's benign — the
    // reload reads back the same content we just wrote and re-applies it
    // idempotently, so it can't oscillate; it's at worst one redundant repaint.
}

/// Parse `--config-dir <path>` (or `--config-dir=<path>`) from the CLI and pin
/// it as the process config directory before anything reads config. Lets a dev
/// build keep its state in a throwaway folder — see the `dev` cargo alias.
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

/// Merge two `:`-separated PATH lists, primary entries first, deduped, empties
/// dropped. Pure so it can be unit-tested; the env write stays in the caller.
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

/// GUI apps launched from Finder/Dock inherit Launch Services' minimal PATH
/// (`/usr/bin:/bin:/usr/sbin:/sbin`), not the user's shell PATH — so the
/// completion engine's `$PATH` scan (`terminal::completion`) can't see
/// Homebrew/cargo/… executables and command candidates silently vanish. Ask the
/// user's login shell for its PATH once and merge it in front of ours (current
/// entries are kept: terminal launches may carry extras like direnv paths).
/// Login-but-not-interactive (`-l -c`) keeps it cheap: zsh reads .zprofile, not
/// .zshrc. Shells spawned by the daemon are unaffected either way — they are
/// login shells and rebuild PATH themselves.
#[cfg(unix)]
fn enrich_path_from_login_shell() {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    // fish prints `$PATH` space-separated; ask it to join with ':' explicitly.
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
    // SAFETY: called from `main` before any thread is spawned, so no concurrent
    // getenv can race the write.
    unsafe { std::env::set_var("PATH", merged) };
}

fn main() {
    // Resolve the config directory override (if any) up front, before any code
    // path touches config/session/history files (the daemon socket path resolves
    // under this dir too, so the order matters).
    apply_config_dir_arg();

    // Daemon mode: when launched with `--daemon` we run the headless persistent
    // terminal server and never open a window. This is the backing process the GUI
    // auto-spawns and reconnects to; it owns all PTYs + child shells and outlives
    // the GUI. Run to completion (the accept loop blocks until killed) then return.
    if std::env::args().any(|a| a == "--daemon") {
        if let Err(e) = crate::daemon::server::run() {
            log::error!("daemon exited with error: {e}");
        }
        return;
    }

    // GUI path: repair the starved Launch Services PATH before anything reads it
    // (completion scans it per keystroke; the daemon we spawn below inherits it).
    #[cfg(unix)]
    enrich_path_from_login_shell();

    // Make sure the persistent daemon is up before we open a window, so the
    // very first RemoteTerminal can connect. This auto-spawns a detached
    // daemon if none is running (sharing our config dir). Failure is non-fatal —
    // we log and continue; a still-absent daemon will surface later when a
    // RemoteTerminal fails to connect, rather than blocking startup here.
    if let Err(e) = crate::daemon::spawn::ensure_running() {
        log::error!("failed to ensure daemon is running: {e}");
    }

    // Register the bundled icon/font asset source so gpui-component `Icon`s
    // (tab glyphs, sidebar icons, etc.) can actually load their SVGs.
    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            register_bundled_fonts(cx);
            cx.activate(true);
            // Load user config once and stash it as a global for views to read.
            cx.set_global(Config::load());
            // Honor `mouse_hide_while_typing` from the start.
            crate::ui::theme::apply_cursor_hide_mode(cx);
            // Start watching `config.json` so edits hot-reload theme/colors live.
            spawn_config_watcher(cx);
            keymap::init(cx);

            cx.spawn(async move |cx| {
                // Open at a roomy default, centred on the primary display (`centered`
                // needs `&App`, which the async cx hands out via `update`).
                let default_size = size(px(1440.), px(900.));
                let bounds = cx.update(|cx| Bounds::centered(None, default_size, cx));
                // Launch state from config: a normal centered window, or maximized /
                // fullscreen. Each variant still carries the centered bounds as the
                // size to restore to when the user un-maximizes / exits fullscreen.
                let startup_mode = cx.update(|cx| cx.global::<Config>().startup_mode);
                let window_bounds = match startup_mode {
                    crate::core::config::StartupMode::Normal => WindowBounds::Windowed(bounds),
                    crate::core::config::StartupMode::Maximized => WindowBounds::Maximized(bounds),
                    crate::core::config::StartupMode::Fullscreen => {
                        WindowBounds::Fullscreen(bounds)
                    }
                };
                let options = WindowOptions {
                    window_bounds: Some(window_bounds),
                    // Start from the component defaults but nudge the traffic lights
                    // down so they stay vertically centred in our taller (40px) title
                    // bar — see `TitleBar::new().h(..)` in `app.rs`. `apply_theme`
                    // re-pins the same position after appearance changes.
                    titlebar: Some(TitlebarOptions {
                        traffic_light_position: Some(crate::ui::theme::traffic_light_position()),
                        ..TitleBar::title_bar_options()
                    }),
                    ..Default::default()
                };

                cx.open_window(options, |window, cx| {
                    let app = cx.new(|cx| Tty7App::new(window, cx));
                    cx.new(|cx| Root::new(app, window, cx).bg(cx.theme().background))
                })
                .expect("failed to open window");
            })
            .detach();
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
        // A starved LS PATH gains the login entries up front.
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

//! The window shell: a transparent unified title bar carrying the tab strip,
//! with the active terminal filling the rest. Owns all tabs (each its own PTY).

use gpui::{App, Axis, Context, Entity, PromptLevel, Subscription, Window, div, prelude::*, px};
use gpui_component::color_picker::{ColorPickerEvent, ColorPickerState};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::select::{SearchableVec, SelectEvent, SelectState};
use gpui_component::slider::{SliderEvent, SliderState};
use gpui_component::{ActiveTheme as _, IndexPath, TitleBar};
use std::sync::Arc;

use crate::core::actions::*;
use crate::core::config::{Config, NewTabPosition, ShellConfig};
use crate::core::session::{Session, SessionAxis, SessionPane, SessionTab};
use crate::core::shells::DetectedShell;
use crate::core::ssh_config;
use crate::daemon::protocol::{
    LoopbackForwardId, LoopbackForwardInfo, RemoteContext, ShellSpec, SshSpec,
    ssh_option_takes_value,
};
use crate::terminal::view::{ChildExited, TerminalView};
use crate::ui::palette::{Command, CommandKind, PaletteEvent, PaletteView};
use crate::ui::pane::{CloseOutcome, Dir, Pane};
use crate::ui::presets::Fill;
use crate::ui::settings::{
    Recording, SettingsSection, SettingsState, ThemeEditor, humanize_action,
};
use crate::ui::theme::{apply_theme, set_menus};

/// One editable color of a user theme, targeted by the in-app color editor. Maps
/// a picker to the seed field (or ANSI slot) it writes back to the theme's file.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemeEdit {
    Background,
    Foreground,
    Accent,
    Cursor,
    Selection,
    Ansi(usize),
}

/// Convert a picked `Hsla` to a `0xRRGGBB` value (alpha dropped) for storage in a
/// theme file.
fn hsla_to_u32(color: gpui::Hsla) -> u32 {
    let rgba: gpui::Rgba = color.into();
    let to = |f: f32| (f.clamp(0.0, 1.0) * 255.0).round() as u32;
    (to(rgba.r) << 16) | (to(rgba.g) << 8) | to(rgba.b)
}

/// Global font-size bounds and step for the live zoom actions.
const FONT_SIZE_MIN: f32 = 6.0;
const FONT_SIZE_MAX: f32 = 48.0;
pub(crate) const FONT_SIZE_STEP: f32 = 1.0;

/// Line-height multiplier bounds and step for the Typography setting. 1.0 packs
/// rows flush against each other; 2.0 is very airy. 1.35 is the default.
const LINE_HEIGHT_MIN: f32 = 1.0;
const LINE_HEIGHT_MAX: f32 = 2.0;
pub(crate) const LINE_HEIGHT_STEP: f32 = 0.05;

/// Cap on the recently-closed-tab stack, bounding memory and the JSON we'd
/// otherwise keep growing without limit.
const MAX_CLOSED_TABS: usize = 20;

/// How much one resize step nudges a split's ratio (see `resize_pane`). Matches
/// the divider's clamp band granularity in `pane.rs`.
const RESIZE_STEP: f32 = 0.05;

/// Quiet window after the last captured chord before a recorded shortcut is
/// committed (see `schedule_recording_commit`). Long enough to type a second
/// chord of a sequence (`ctrl-b x`), short enough that a single chord commits
/// promptly.
const RECORD_COMMIT_DELAY_MS: u64 = 650;

/// One tab: a split-pane tree plus an optional user-assigned name.
pub struct Tab {
    /// The tab's split-pane tree (one or more terminals). For a settings tab
    /// this is `Pane::Empty` — the body renders the settings panel instead.
    pub pane: Pane,
    /// User-set custom name (via "Rename Tab"). `None` → derive the label from
    /// the focused terminal's title at render time.
    pub name: Option<String>,
    /// `Some` for the dedicated settings tab; `None` for a normal terminal tab.
    /// A settings tab carries its own panel state and is never persisted.
    settings: Option<SettingsState>,
}

impl Tab {
    fn new(pane: Pane) -> Self {
        Self {
            pane,
            name: None,
            settings: None,
        }
    }

    /// True for the dedicated settings tab.
    pub(crate) fn is_settings(&self) -> bool {
        self.settings.is_some()
    }

    /// The title of the tab's representative terminal (its first leaf), used to
    /// derive both the tab label and its context icon. Empty when there's no
    /// terminal or no title yet.
    pub(crate) fn leaf_title(&self, cx: &App) -> String {
        self.pane
            .first_leaf()
            .map(|l| l.read(cx).title.clone())
            .unwrap_or_default()
    }
}

/// In-progress inline rename of a tab (double-click a tab label). Holds the
/// gpui-component text input plus the subscriptions that commit it on Enter/Blur.
pub(crate) struct Renaming {
    /// Index of the tab being renamed, in `Tty7App::tabs`.
    pub(crate) index: usize,
    pub(crate) input: Entity<InputState>,
    _subs: Vec<Subscription>,
}

pub(crate) struct LoopbackForwardPanelState {
    pub(crate) open_pane_id: Option<u64>,
    pub(crate) forwards: Vec<LoopbackForwardInfo>,
    pub(crate) host_input: Entity<InputState>,
    pub(crate) port_input: Entity<InputState>,
    pub(crate) editing: Option<LoopbackForwardId>,
}

pub struct Tty7App {
    /// The open tabs; each owns a split-pane tree and an optional name.
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active: usize,
    /// Current global font size (px), applied to every pane in every tab.
    pub(crate) font_size: f32,
    /// Current global line-height multiplier, applied to every pane.
    pub(crate) line_height: f32,
    /// Currently-applied font family. Tracked (not just read from config on
    /// demand) so the `Config`-global observer can tell a hot-reloaded family
    /// change from the far more common no-op re-notify.
    pub(crate) font_family: String,
    /// Currently-applied distinct bold/italic faces (`None` = synthesized), also
    /// tracked so the hot-reload observer can diff them like `font_family`.
    pub(crate) font_family_bold: Option<String>,
    pub(crate) font_family_italic: Option<String>,
    /// Currently-applied OpenType features for terminal fonts. `None` means the
    /// terminal-safe default (ligatures disabled).
    pub(crate) font_features: Option<gpui::FontFeatures>,
    /// Keeps the `observe_global::<Config>` subscription alive for the app's
    /// lifetime so external edits to `config.json` (swapped in by the watcher in
    /// `main.rs`) live-apply font size / line height / family. Never read.
    _config_watch: Subscription,
    /// Keeps the keystroke interceptor alive: any real keypress cancels the
    /// held-⌘/Ctrl tab badges (and any pending reveal), so a chord like ⌘C
    /// never shows them — only a bare hold does. An *interceptor* (fires
    /// pre-dispatch) rather than an observer because the terminal consumes
    /// most keys with `stop_propagation`, which suppresses observers. Never read.
    _keystroke_watch: Subscription,
    /// Keeps the window-activation observer alive: any active-status flip also
    /// cancels the badges. Deactivating mid-hold (⌘-Tab, Spotlight, a click
    /// into another app) sends the modifier *release* to whatever app is key
    /// by then — this window never gets that `ModifiersChanged`, so without
    /// this the badges stuck on until some later keypress. Never read.
    _activation_watch: Subscription,
    /// `Some` while the command palette overlay is open; `None` when closed.
    /// The view owns its search input, filtered list and keyboard handling and
    /// emits a `PaletteEvent`; we build the catalog and run the chosen command.
    palette: Option<Entity<PaletteView>>,
    /// Keeps the open palette's event subscription alive; dropped on close.
    palette_sub: Option<Subscription>,
    /// Stack of recently closed tabs (most recent on top) for Cmd+Shift+T.
    /// Stored serialized so each entry carries the panes' cwd + name at close.
    /// `pub(crate)` so the home page can surface the top entry as its
    /// "reopen what you just closed" hint.
    pub(crate) closed: Vec<SessionTab>,
    /// `Some` while a tab label is being renamed inline; `None` otherwise.
    pub(crate) renaming: Option<Renaming>,
    /// When `Some`, the active tab renders only this one leaf full-window
    /// (Cmd+Shift+Enter maximize). Cleared on any structural / navigation change.
    maximized: Option<Entity<TerminalView>>,
    /// Whether the tab chips currently show their ⌘1…⌘9 switch badges
    /// (shown while bare ⌘/Ctrl is held; see `hints::on_modifiers_changed`).
    pub(crate) mod_hint_badges: bool,
    /// Generation counter for the delayed badge reveal: bumped on every
    /// modifier transition and keypress so a stale timer can't fire.
    pub(crate) mod_hint_gen: u64,
    /// Generation counter for the keybinding-capture commit timer: bumped on
    /// every captured chord, cancel, and start, so a stale pause-to-commit
    /// timer can't fire after the sequence changed or capture ended.
    record_gen: u64,
    /// Focus target for the home page (the zero-tab state; see `ui::home`).
    /// Keeping something focused keeps keystrokes flowing through the window's
    /// dispatch path, so ⌘T & friends still reach the root action handlers.
    pub(crate) home_focus: gpui::FocusHandle,
    /// Shells found on this machine (`core::shells::detect_shells`), listed in
    /// the "+" dropdown. Probed once at startup off the UI thread — empty until
    /// that lands, when the dropdown offers just the default entry.
    pub(crate) detected_shells: Vec<DetectedShell>,
    /// Pane-contextual SSH loopback forward UI state. The controls render only
    /// over the active SSH pane, but the input/editing state is app-owned so it
    /// is not tied to the Settings tab.
    pub(crate) loopback_panel: LoopbackForwardPanelState,
}

impl Tty7App {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Restore the previous session (tab/split layout + each pane's cwd).
        Self::with_session(Session::load(), window, cx)
    }

    /// The whole constructor behind `new`, with the saved session injected
    /// instead of read from disk. The headless tests build the app through
    /// this seam (a zero-tab session → the home page, no terminal spawned)
    /// so every subscription and window hook runs exactly as in production
    /// without touching `~/.config` or a daemon.
    pub(crate) fn with_session(
        session: Option<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Font size from config (borrow ends before the mutable theme apply).
        let font_size = cx.global::<Config>().font_size;
        let line_height = cx.global::<Config>().line_height;
        let font_family = cx.global::<Config>().font_family.clone();
        let font_family_bold = cx.global::<Config>().font_family_bold.clone();
        let font_family_italic = cx.global::<Config>().font_family_italic.clone();
        let font_features = cx.global::<Config>().font_features.clone();
        let loopback_host_input =
            cx.new(|cx| InputState::new(window, cx).default_value("localhost"));
        let loopback_port_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("3000")
                .default_value("")
        });
        // Live-apply hot-reloaded config: the watcher in `main.rs` swaps the
        // `Config` global on every `config.json` change, which fires this. Theme
        // and colors are handled separately by `apply_theme`; here we cover the
        // font knobs that live on `Tty7App`/the panes.
        let config_watch = cx.observe_global::<Config>(|this, cx| this.reload_from_config(cx));
        // Any real keypress means "chord, not a bare hold": cancel the held-⌘
        // tab badges and whatever reveal is pending (see `ui::hints`).
        let this = cx.weak_entity();
        let keystroke_watch = cx.intercept_keystrokes(move |_ev, _window, cx| {
            let _ = this.update(cx, |this, cx| this.dismiss_mod_hint(cx));
        });
        // Losing key status mid-hold (⌘-Tab, Spotlight, a click into another
        // app) means the modifier release is delivered elsewhere and never
        // reaches this window — the activation flip is the only signal left,
        // so treat it like a release. Dismissing on *both* flips also keeps a
        // reveal scheduled just before the switch from popping the badges up
        // in a window the user already left.
        let activation_watch = cx.observe_window_activation(window, |this, _window, cx| {
            this.dismiss_mod_hint(cx);
            // The panes' link-modifier tracking loses the release the same
            // way, and a stale "⌘ held" is worse than missing badges: a
            // plain unmodified click would open links. Treat the flip as a
            // release; holding ⌘ again re-arms it via `on_modifiers_changed`.
            this.set_link_modifier(false, cx);
        });
        // Paint the configured color theme (defaults to a light one) and build
        // the menu bar.
        apply_theme(Some(window), cx);
        set_menus(cx);
        // A session with zero tabs is a real state — the user quit from
        // the home page — and restores back to it; only a *missing/unreadable*
        // session (first run) falls back to spawning a default terminal.
        let (tabs, active) = match session {
            // First run (no session file): the very first terminal has no
            // predecessor to inherit from, so start in the app's current
            // directory (None → default behavior).
            None => {
                let first = new_terminal(font_size, None, None, None, window, cx);
                (vec![Tab::new(Pane::leaf(first))], 0)
            }
            // A saved session (with tabs, or an empty home-page state): rebuild it
            // the same way a daemon restart does.
            some => tabs_from_session(some, font_size, window, cx),
        };
        let app = Self {
            tabs,
            active,
            font_size,
            line_height,
            font_family,
            font_family_bold,
            font_family_italic,
            font_features,
            _config_watch: config_watch,
            _keystroke_watch: keystroke_watch,
            _activation_watch: activation_watch,
            palette: None,
            palette_sub: None,
            closed: Vec::new(),
            renaming: None,
            maximized: None,
            mod_hint_badges: false,
            mod_hint_gen: 0,
            record_gen: 0,
            home_focus: cx.focus_handle(),
            detected_shells: Vec::new(),
            loopback_panel: LoopbackForwardPanelState {
                open_pane_id: None,
                forwards: crate::terminal::RemoteTerminal::list_loopback_forwards(),
                host_input: loopback_host_input,
                port_input: loopback_port_input,
                editing: None,
            },
        };
        // Discover this machine's shells for the "+" dropdown off the UI thread
        // (the WSL probe on Windows spawns a process, and /etc/shells hits the
        // filesystem). Until it lands the dropdown offers just the default entry.
        cx.spawn(async move |this, cx| {
            let shells = cx
                .background_spawn(async { crate::core::shells::detect_shells() })
                .await;
            // `notify` so the strip re-renders and the dropdown closure
            // captures the freshly landed list (nothing else is guaranteed to
            // redraw an idle window).
            let _ = this.update(cx, |app, cx| {
                app.detected_shells = shells;
                cx.notify();
            });
        })
        .detach();
        // Persist the session one last time as the app quits. This captures the
        // latest state — including a plain `cd` that changed a pane's cwd but
        // triggered no structural change — so the next launch restores where the
        // user actually left off. The callback gets the live `Tty7App`, reads
        // every pane's current cwd, and writes the file synchronously; the empty
        // future just satisfies the hook's async signature. The subscription is
        // detached to live for the app's lifetime (its weak handle keeps it safe
        // after teardown).
        cx.on_app_quit(|app, cx| {
            app.save_session(cx);
            async move {}
        })
        .detach();

        // Confirm before the red traffic light closes the window. Closing quits
        // the app, but the panes are *detached, not killed* — they keep running in
        // the daemon and re-attach on the next launch — so the prompt reassures
        // rather than warns. We veto the immediate close (return `false`), show the
        // prompt, and quit only if the user picks "Close". A one-shot flag lets
        // that post-confirm quit through should we be asked again, instead of
        // looping the prompt.
        let close_confirmed = std::rc::Rc::new(std::cell::Cell::new(false));
        let weak_app = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            if close_confirmed.get() {
                return true;
            }
            // From the home page (zero tabs) there are no running sessions to
            // reassure about — prompting would be pure friction. Close directly.
            if weak_app
                .upgrade()
                .is_some_and(|app| app.read(cx).tabs.is_empty())
            {
                return true;
            }
            let answer = window.prompt(
                PromptLevel::Info,
                "Close Window?",
                Some(
                    "Your sessions keep running in the background and will be \
                     restored the next time you open tty7.",
                ),
                &["Cancel", "Close"],
                cx,
            );
            let close_confirmed = close_confirmed.clone();
            cx.spawn(async move |cx| {
                // Index 1 == "Close"; index 0 (Cancel) and a dismissed prompt
                // both leave the window open.
                if let Ok(1) = answer.await {
                    close_confirmed.set(true);
                    cx.update(|cx| cx.quit());
                }
            })
            .detach();
            false
        });

        app.focus_active(window, cx);
        app
    }

    /// Snapshot the current tabs/active index into a `Session` and persist it.
    /// Called after every structural change; the write is a small synchronous
    /// JSON dump and any error is swallowed inside `Session::save`.
    fn save_session(&self, cx: &App) {
        // The settings tab is ephemeral — exclude it, and clamp `active` into the
        // remaining terminal tabs so the next launch restores a real tab.
        let tabs: Vec<SessionTab> = self
            .tabs
            .iter()
            .filter(|tab| !tab.is_settings())
            .map(|tab| tab_to_session(tab, cx))
            .collect();
        // Zero terminal tabs is a real state (the home page) and is persisted as
        // such, so the next launch comes back to it instead of a fresh shell.
        if tabs.is_empty() {
            Session::default().save();
            return;
        }
        // Remap `self.active` (an index into the *unfiltered* tabs) into the
        // filtered list: it's the number of non-settings tabs before it. A plain
        // `min` clamp is wrong when the settings tab sits *before* the active one
        // — it would shift the restored selection onto the wrong tab.
        let active = self.tabs[..self.active.min(self.tabs.len())]
            .iter()
            .filter(|tab| !tab.is_settings())
            .count()
            .min(tabs.len() - 1);
        let session = Session { active, tabs };
        session.save();
    }

    /// Reopen the most recently closed tab (Cmd+Shift+T). Rebuilds its pane
    /// tree (restoring each terminal's saved cwd), inserts it after the active
    /// tab, and focuses it. No-op when the stack is empty.
    fn reopen_closed_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(st) = self.closed.pop() else {
            return;
        };
        let alive = alive_panes();
        let pane = session_to_pane(&st.pane, &alive, self.font_size, window, cx);
        self.maximized = None;
        let insert_at = self.new_tab_insert_at(cx);
        self.tabs.insert(
            insert_at,
            Tab {
                pane,
                name: st.name,
                settings: None,
            },
        );
        self.active = insert_at;
        self.focus_active(window, cx);
        self.save_session(cx);
        cx.notify();
    }

    /// Restart the persistent background daemon: shut the running one down (which
    /// stops every live shell) and bring a fresh one up, then rebuild the tabs
    /// from the just-saved session so the layout returns with fresh shells.
    ///
    /// A general escape hatch for the otherwise invisible, always-on daemon:
    /// picking up a macOS permission granted after it started (Full Disk Access
    /// and the like only reach it on a fresh process), recovering if it wedges, or
    /// just starting from a clean slate — none of which quitting/reopening the GUI
    /// achieves, since that leaves the detached daemon untouched. Guarded by a
    /// confirmation because it ends running sessions. The shutdown + respawn runs
    /// off the UI thread (the daemon hangs up each child with a short grace, so it
    /// can take a beat); the tab rebuild hops back to the main thread, where it has
    /// the `Window`.
    pub(crate) fn restart_daemon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            "Restart Daemon?",
            Some(
                "This stops every running terminal session — anything still \
                 running in them will be terminated. Your tabs and layout are kept \
                 and reopened with fresh shells.",
            ),
            &["Cancel", "Restart"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            // Index 1 == "Restart"; Cancel or a dismissed prompt leave everything
            // running untouched.
            if !matches!(answer.await, Ok(1)) {
                return;
            }
            // Persist the current layout + cwds, then tear the live terminals down
            // *before* the daemon dies: dropping each `RemoteTerminal` detaches its
            // socket, so no reader thread is mid-read when the daemon exits. The
            // window briefly shows the empty home page while the daemon restarts.
            if this
                .update_in(cx, |this, _window, cx| {
                    this.save_session(cx);
                    this.maximized = None;
                    this.tabs.clear();
                    this.active = 0;
                    cx.notify();
                })
                .is_err()
            {
                return;
            }
            // Shut the old daemon down and spawn a fresh one off the UI thread.
            let restarted = cx
                .background_spawn(async move { crate::daemon::spawn::restart() })
                .await;
            // Rebuild from the saved session. The fresh daemon has no live panes,
            // so every leaf spawns a new shell in its saved cwd and the tab/split
            // layout returns exactly as it was.
            let _ = this.update_in(cx, |this, window, cx| {
                match &restarted {
                    Ok(()) => {
                        let font_size = this.font_size;
                        let (tabs, active) =
                            tabs_from_session(Session::load(), font_size, window, cx);
                        this.tabs = tabs;
                        this.active = active;
                    }
                    // The fresh daemon never came up; rebuilding would panic in
                    // `new_terminal`'s connect `.expect`. Stay on the home page and
                    // leave a breadcrumb rather than crash — the user can retry.
                    Err(e) => {
                        log::error!("restart background service failed, staying on home page: {e}");
                    }
                }
                this.focus_active(window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Apply `size` (clamped) as the new global font size across every pane.
    /// The element re-measures cell geometry next frame, so the grid reflows
    /// automatically once each view is notified.
    fn set_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        let size = size.clamp(FONT_SIZE_MIN, FONT_SIZE_MAX);
        self.font_size = size;
        let px_size = px(size);
        for tab in &self.tabs {
            for leaf in tab.pane.leaves() {
                leaf.update(cx, |v, cx| {
                    v.font_size = px_size;
                    cx.notify();
                });
            }
        }
        // Persist so the zoom level survives a restart.
        let cfg = cx.global_mut::<Config>();
        cfg.font_size = size;
        cfg.save();
        cx.notify();
    }

    pub(crate) fn change_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.set_font_size(self.font_size + delta, cx);
    }

    /// Reset the global font size back to the built-in default. We use the
    /// compiled-in default rather than `config.font_size`, because the latter now
    /// tracks the live zoom level (persisted on every change), so it no longer
    /// serves as a stable reset target.
    pub(crate) fn reset_font_size(&mut self, cx: &mut Context<Self>) {
        self.set_font_size(Config::default().font_size, cx);
    }

    /// Apply `mul` (clamped) as the new global line-height multiplier across every
    /// pane. Like `set_font_size`, the element re-derives row height next frame, so
    /// the grid reflows once each view is notified.
    fn set_line_height(&mut self, mul: f32, cx: &mut Context<Self>) {
        let mul = mul.clamp(LINE_HEIGHT_MIN, LINE_HEIGHT_MAX);
        self.line_height = mul;
        for tab in &self.tabs {
            for leaf in tab.pane.leaves() {
                leaf.update(cx, |v, cx| {
                    v.line_height_mul = mul;
                    cx.notify();
                });
            }
        }
        // Persist so the spacing survives a restart.
        let cfg = cx.global_mut::<Config>();
        cfg.line_height = mul;
        cfg.save();
        cx.notify();
    }

    pub(crate) fn change_line_height(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.set_line_height(self.line_height + delta, cx);
    }

    /// Reset the line-height multiplier back to the built-in default (see the note
    /// on `reset_font_size`: config now tracks the live value, not a reset target).
    pub(crate) fn reset_line_height(&mut self, cx: &mut Context<Self>) {
        self.set_line_height(Config::default().line_height, cx);
    }

    /// Switch the active color theme by id, repaint, and persist the choice so
    /// it survives a restart. The theme carries its own dark/light brightness.
    pub(crate) fn set_preset(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        cx.global_mut::<Config>().theme_preset = id.to_string();
        apply_theme(Some(window), cx);
        set_menus(cx);
        cx.global::<Config>().save();
        // The editor targets the active theme, so its pickers must track a switch.
        self.rebuild_theme_editor(window, cx);
        cx.notify();
    }

    /// Show/hide the theme picker panel beside the Appearance page.
    pub(crate) fn toggle_theme_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.active_settings_mut() {
            s.theme_panel_open = !s.theme_panel_open;
            cx.notify();
        }
    }

    /// Close the theme picker panel (its `×`).
    pub(crate) fn close_theme_panel(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.active_settings_mut() {
            s.theme_panel_open = false;
            cx.notify();
        }
    }

    /// Open the user themes folder (`~/.config/tty7/themes`) in the system file
    /// browser, creating it first so there's always somewhere to drop a theme.
    pub(crate) fn open_themes_folder(&self, cx: &mut Context<Self>) {
        if let Some(dir) = crate::ui::presets::themes_dir() {
            let _ = std::fs::create_dir_all(&dir);
            cx.open_with_system(&dir);
        }
    }

    /// Duplicate the active theme into an editable YAML file, switch to the copy,
    /// and open the color editor on it. This is the entry point for customizing a
    /// read-only built-in (or an imported iTerm scheme).
    pub(crate) fn fork_active_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = cx.global::<Config>().theme_preset.clone();
        let theme = crate::ui::presets::by_id(cx, &id);
        match crate::ui::presets::fork_to_file(&theme) {
            Ok(new_id) => {
                crate::ui::presets::load_registry(cx);
                // Switches to the copy (applies + persists + rebuilds the editor).
                self.set_preset(&new_id, window, cx);
            }
            Err(e) => log::warn!("failed to duplicate theme: {e}"),
        }
    }

    /// Apply one color edit to the active (editable) theme: mutate the seed field,
    /// write the theme's file, reload the registry, and repaint live.
    pub(crate) fn edit_active_theme(
        &mut self,
        edit: ThemeEdit,
        value: gpui::Hsla,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = cx.global::<Config>().theme_preset.clone();
        let mut theme = crate::ui::presets::by_id(cx, &id);
        if !theme.editable() {
            return;
        }
        let c = hsla_to_u32(value);
        match edit {
            ThemeEdit::Background => theme.background = Fill::Solid(c),
            ThemeEdit::Foreground => theme.foreground = c,
            ThemeEdit::Accent => theme.accent = c,
            ThemeEdit::Cursor => theme.caret = Some(c),
            ThemeEdit::Selection => theme.selection = Some(c),
            ThemeEdit::Ansi(i) => theme.ansi16[i] = ((c >> 16) as u8, (c >> 8) as u8, c as u8),
        }
        if let Err(e) = crate::ui::presets::write_theme_file(&theme) {
            log::warn!("failed to write theme file: {e}");
            return;
        }
        crate::ui::presets::load_registry(cx);
        apply_theme(Some(window), cx);
        cx.notify();
    }

    /// (Re)build the settings tab's color-editor pickers for the current active
    /// theme. If no settings tab is open or the active theme isn't an editable
    /// file, the editor is cleared. Called after every theme switch / duplicate
    /// and when opening settings, so the pickers always reflect (and target) the
    /// theme currently on screen.
    pub(crate) fn rebuild_theme_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(idx) = self.settings_tab_index() else {
            return;
        };
        let id = cx.global::<Config>().theme_preset.clone();
        let theme = crate::ui::presets::by_id(cx, &id);
        if !theme.editable() {
            if let Some(s) = self.tabs.get_mut(idx).and_then(|t| t.settings.as_mut()) {
                s.theme_editor = None;
            }
            return;
        }

        let neutrals = theme.neutrals();
        // (edit target, row label, current 0xRRGGBB value) for each seed color.
        let seed_specs: [(ThemeEdit, &str, u32); 5] = [
            (
                ThemeEdit::Background,
                "Background",
                theme.background_color(),
            ),
            (ThemeEdit::Foreground, "Foreground", theme.foreground),
            (ThemeEdit::Accent, "Accent", theme.accent),
            (
                ThemeEdit::Cursor,
                "Cursor",
                theme.caret.unwrap_or(theme.accent),
            ),
            (ThemeEdit::Selection, "Selection", neutrals.selection),
        ];

        let mut subs = Vec::new();
        let mut make =
            |edit: ThemeEdit, value: u32, subs: &mut Vec<Subscription>, cx: &mut Context<Self>| {
                let eff: gpui::Hsla = gpui::rgb(value).into();
                let state = cx.new(|cx| ColorPickerState::new(window, cx).default_value(eff));
                subs.push(cx.subscribe_in(
                    &state,
                    window,
                    move |this, _picker, ev: &ColorPickerEvent, window, cx| {
                        let ColorPickerEvent::Change(value) = ev;
                        if let Some(v) = value {
                            this.edit_active_theme(edit, *v, window, cx);
                        }
                    },
                ));
                state
            };

        let seed = seed_specs
            .iter()
            .map(|&(edit, label, value)| {
                (edit, label.to_string(), make(edit, value, &mut subs, cx))
            })
            .collect();
        let ansi = (0..16)
            .map(|i| {
                let (r, g, b) = theme.ansi16[i];
                let value = (r as u32) << 16 | (g as u32) << 8 | b as u32;
                (
                    ThemeEdit::Ansi(i),
                    format!("Color {i}"),
                    make(ThemeEdit::Ansi(i), value, &mut subs, cx),
                )
            })
            .collect();

        if let Some(s) = self.tabs.get_mut(idx).and_then(|t| t.settings.as_mut()) {
            s.theme_editor = Some(ThemeEditor {
                for_id: theme.id.clone(),
                seed,
                ansi,
                _subs: subs,
            });
        }
    }

    /// Toggle terminal font ligatures through the generic `font_features`
    /// config. On enables the common programming-font features; off restores
    /// tty7's terminal-safe default (contextual ligatures disabled).
    pub(crate) fn set_font_ligatures(&mut self, on: bool, cx: &mut Context<Self>) {
        let features = on.then(|| {
            gpui::FontFeatures(Arc::new(vec![
                ("calt".to_string(), 1),
                ("liga".to_string(), 1),
            ]))
        });
        self.font_features = features.clone();
        for tab in &self.tabs {
            for leaf in tab.pane.leaves() {
                let features = features.clone();
                leaf.update(cx, |v, cx| v.set_font_features(features, cx));
            }
        }
        let cfg = cx.global_mut::<Config>();
        cfg.font_features = features;
        cfg.save();
        cx.notify();
    }

    /// Switch the cursor shape and repaint. The element reads `cursor_style` from
    /// the global each frame, so we just persist and nudge every pane to redraw.
    pub(crate) fn set_cursor_style(
        &mut self,
        style: crate::core::config::CursorStyle,
        cx: &mut Context<Self>,
    ) {
        self.update_config(cx, |cfg| cfg.cursor_style = style);
        for tab in &self.tabs {
            for leaf in tab.pane.leaves() {
                leaf.update(cx, |_v, cx| cx.notify());
            }
        }
    }

    // ── Config setters (Terminal / Window & Tabs / Cursor settings) ─────────
    // Each goes through `update_config` (mutate the global, persist, repaint).
    // Effect points read the global live (blink task, `poll_foreground`, link
    // gates, `new_tab_insert_at`), so there's nothing to push into the panes —
    // except cursor blink, which must un-hide a cursor a prior blink cycle may
    // have left dark.

    /// Shared tail of every config setter: mutate the global `Config`, persist
    /// it, and repaint so the control reflects the new value. Keeping the
    /// persist/notify contract here means a future change (e.g. debounced
    /// saves) lands in one place.
    fn update_config(&mut self, cx: &mut Context<Self>, mutate: impl FnOnce(&mut Config)) {
        let cfg = cx.global_mut::<Config>();
        mutate(cfg);
        cfg.save();
        cx.notify();
    }

    pub(crate) fn set_link_url(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.link_url = on);
    }

    pub(crate) fn set_ssh_loopback_forward(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.ssh_loopback_forward = on);
    }

    pub(crate) fn refresh_loopback_forwards(&mut self, cx: &mut Context<Self>) {
        self.loopback_panel.forwards = crate::terminal::RemoteTerminal::list_loopback_forwards();
        cx.notify();
    }

    pub(crate) fn close_loopback_forward(&mut self, id: LoopbackForwardId, cx: &mut Context<Self>) {
        self.loopback_panel.forwards = crate::terminal::RemoteTerminal::close_loopback_forward(id);
        cx.notify();
    }

    pub(crate) fn toggle_loopback_forward_panel(&mut self, pane_id: u64, cx: &mut Context<Self>) {
        let should_open = self.loopback_panel.open_pane_id != Some(pane_id);
        if should_open {
            self.loopback_panel.open_pane_id = Some(pane_id);
            if self
                .loopback_panel
                .editing
                .as_ref()
                .is_some_and(|id| id.pane_id != pane_id)
            {
                self.loopback_panel.editing = None;
            }
            self.refresh_loopback_forwards(cx);
        } else {
            self.loopback_panel.open_pane_id = None;
            self.loopback_panel.editing = None;
        }
        cx.notify();
    }

    pub(crate) fn close_loopback_forward_panel(&mut self, cx: &mut Context<Self>) {
        self.loopback_panel.open_pane_id = None;
        self.loopback_panel.editing = None;
        cx.notify();
    }

    pub(crate) fn save_loopback_forward_form(
        &mut self,
        pane_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = self
            .loopback_panel
            .host_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        let Some(port) = self
            .loopback_panel
            .port_input
            .read(cx)
            .value()
            .trim()
            .parse::<u16>()
            .ok()
        else {
            return;
        };
        if host.is_empty() {
            return;
        }
        let editing = self.loopback_panel.editing.clone();
        let pane_id = editing.as_ref().map(|id| id.pane_id).unwrap_or(pane_id);

        if crate::terminal::RemoteTerminal::ensure_loopback_forward(pane_id, &host, port).is_ok() {
            if let Some(old) = editing {
                if old.remote_host != host || old.remote_port != port {
                    let _ = crate::terminal::RemoteTerminal::close_loopback_forward(old);
                }
            }
            self.loopback_panel.forwards =
                crate::terminal::RemoteTerminal::list_loopback_forwards();
            self.loopback_panel.editing = None;
            self.loopback_panel.host_input.update(cx, |input, cx| {
                input.set_value("localhost", window, cx);
            });
            self.loopback_panel.port_input.update(cx, |input, cx| {
                input.set_value("", window, cx);
            });
            cx.notify();
        }
    }

    pub(crate) fn edit_loopback_forward(
        &mut self,
        id: LoopbackForwardId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loopback_panel.open_pane_id = Some(id.pane_id);
        self.loopback_panel.editing = Some(id.clone());
        self.loopback_panel.host_input.update(cx, |input, cx| {
            input.set_value(id.remote_host, window, cx);
        });
        self.loopback_panel.port_input.update(cx, |input, cx| {
            input.set_value(id.remote_port.to_string(), window, cx);
        });
        cx.notify();
    }

    pub(crate) fn cancel_loopback_forward_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loopback_panel.editing = None;
        self.loopback_panel.host_input.update(cx, |input, cx| {
            input.set_value("localhost", window, cx);
        });
        self.loopback_panel.port_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
        });
        cx.notify();
    }

    fn open_managed_ssh_spec(&mut self, ssh: SshSpec, window: &mut Window, cx: &mut Context<Self>) {
        if ssh.validate().is_err() {
            return;
        }
        let mut shell_args = ssh.args.clone();
        shell_args.push(ssh.target.clone());
        self.new_tab_with_shell(
            Some(ShellSpec {
                program: "ssh".to_string(),
                args: shell_args,
                ssh: Some(ssh),
            }),
            window,
            cx,
        );
    }

    /// Toggle the startup update check (Settings → About). Takes effect on the
    /// next launch — this only persists the preference; it doesn't run or cancel
    /// an in-flight check.
    pub(crate) fn set_check_for_updates(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.check_for_updates = on);
    }

    pub(crate) fn set_cursor_blink(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.cursor_blink = on);
        // Turning blink off mid-cycle could leave the cursor in its hidden phase;
        // force every pane's cursor back on so it doesn't stick invisible.
        if !on {
            for tab in &self.tabs {
                for leaf in tab.pane.leaves() {
                    leaf.update(cx, |v, cx| {
                        v.cursor_visible = true;
                        cx.notify();
                    });
                }
            }
        }
    }

    pub(crate) fn set_scrollback_limit(&mut self, lines: usize, cx: &mut Context<Self>) {
        // Callers pass fixed in-range presets, but clamp anyway so a future caller
        // can't smuggle in a degenerate value.
        self.update_config(cx, |cfg| {
            cfg.scrollback_limit = lines.clamp(100, crate::core::config::MAX_SCROLLBACK)
        });
    }

    pub(crate) fn set_new_tab_position(&mut self, pos: NewTabPosition, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.new_tab_position = pos);
    }

    pub(crate) fn set_notify_mode(
        &mut self,
        mode: crate::core::config::NotifyMode,
        cx: &mut Context<Self>,
    ) {
        self.update_config(cx, |cfg| cfg.notify_on_command_finish = mode);
    }

    // ── Input / Mouse setters ───────────────────────────────────────────────

    /// Takes effect on the next keystroke — the terminal reads the flag per
    /// key event, so nothing needs pushing to open panes.
    pub(crate) fn set_macos_option_as_alt(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.macos_option_as_alt = on);
    }

    pub(crate) fn set_mouse_hide_while_typing(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.mouse_hide_while_typing = on);
        // Push the new policy to GPUI right away (same call the hot-reload uses).
        crate::ui::theme::apply_cursor_hide_mode(cx);
    }

    pub(crate) fn set_focus_follows_mouse(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.focus_follows_mouse = on);
    }

    pub(crate) fn set_mouse_scroll_multiplier(&mut self, mult: f32, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| {
            cfg.mouse_scroll_multiplier = mult.clamp(0.1, 10.0)
        });
    }

    pub(crate) fn set_clipboard_trim(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.clipboard_trim_trailing_spaces = on);
    }

    pub(crate) fn set_copy_on_select(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.copy_on_select = on);
    }

    pub(crate) fn set_startup_mode(
        &mut self,
        mode: crate::core::config::StartupMode,
        cx: &mut Context<Self>,
    ) {
        self.update_config(cx, |cfg| cfg.startup_mode = mode);
    }

    fn focus_active(&self, window: &mut Window, cx: &mut App) {
        let Some(tab) = self.tabs.get(self.active) else {
            // No tabs → the home page is showing; keep something focused so
            // keystrokes stay on the window's dispatch path (⌘T etc. must still
            // reach the root action handlers).
            window.focus(&self.home_focus, cx);
            return;
        };
        // Settings tab: route focus to its panel so Esc-to-close works.
        if let Some(settings) = tab.settings.as_ref() {
            window.focus(&settings.focus_handle, cx);
        } else if let Some(leaf) = tab.pane.first_leaf() {
            let handle = leaf.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
        }
    }

    fn focus_leaf(&self, leaf: &Entity<TerminalView>, window: &mut Window, cx: &mut App) {
        let handle = leaf.read(cx).focus_handle.clone();
        window.focus(&handle, cx);
    }

    /// Where a freshly opened tab should be inserted, per `new_tab_position`:
    /// right after the active tab, or appended at the end. Clamped to the tab
    /// count so the zero-tab home state (active 0, no tabs) inserts at 0.
    fn new_tab_insert_at(&self, cx: &App) -> usize {
        match cx.global::<Config>().new_tab_position {
            NewTabPosition::AfterCurrent => (self.active + 1).min(self.tabs.len()),
            NewTabPosition::End => self.tabs.len(),
        }
    }

    pub(crate) fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_tab_with_shell(None, window, cx);
    }

    /// Open a new tab running `shell` — a pick from the "+" dropdown — or the
    /// default shell when `None` (the plain "+" click / Cmd+T path).
    pub(crate) fn new_tab_with_shell(
        &mut self,
        shell: Option<ShellSpec>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Inherit the cwd of the active tab's focused terminal so the new tab
        // opens in the same directory the user is currently working in.
        let cwd = self.tabs.get(self.active).and_then(|t| {
            t.pane
                .focused_or_first(window, cx)
                .and_then(|leaf| leaf.read(cx).cwd())
        });
        let tab = new_terminal(self.font_size, cwd, None, shell, window, cx);
        self.maximized = None;
        let insert_at = self.new_tab_insert_at(cx);
        self.tabs.insert(insert_at, Tab::new(Pane::leaf(tab)));
        self.active = insert_at;
        self.focus_active(window, cx);
        self.save_session(cx);
        cx.notify();
    }

    /// Split the focused pane in the active tab, focusing the new terminal.
    pub(crate) fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        // Capture the target leaf BEFORE creating the new terminal: constructing
        // a TerminalView focuses it, which would otherwise make us lose track of
        // which pane to split (nested splits would always hit the first leaf).
        let Some(target) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
        else {
            return;
        };
        // The new pane inherits the cwd — and the shell, when the pane being
        // split was opened with an explicit pick (a WSL/fish tab splits into
        // more WSL/fish, not back to the default).
        let cwd = target.read(cx).cwd();
        let shell = target.read(cx).shell_spec();
        let new = new_terminal(self.font_size, cwd, None, shell, window, cx);
        if let Some(tab) = self.tabs.get_mut(self.active) {
            if tab.pane.split_leaf(&target, axis, new.clone()) {
                self.maximized = None;
                self.focus_leaf(&new, window, cx);
                self.save_session(cx);
                cx.notify();
            }
        }
    }

    /// Close the focused pane. If it was the tab's only pane, close the tab.
    fn close_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.maximized = None;
        // Capture the focused leaf before closing: if a split collapses, that
        // leaf is destroyed with no reopen path, so we kill its daemon pane. Owned
        // clones from `leaves()` end the borrow before the `&mut` close below.
        let focused = self.tabs.get(self.active).and_then(|tab| {
            tab.pane
                .leaves()
                .into_iter()
                .find(|l| l.read(cx).focus_handle.contains_focused(window, cx))
        });
        let outcome = match self.tabs.get_mut(self.active) {
            Some(tab) => tab.pane.close_focused(window, cx),
            None => return,
        };
        match outcome {
            CloseOutcome::RemoveSelf => {
                // The focused leaf *is* the tab's only pane: close the tab, which
                // kills its panes itself.
                self.close_tab(self.active, window, cx);
            }
            CloseOutcome::NotFound => {
                // No terminal leaf in the active tab holds focus (focus is in the
                // rename input / settings / drifted). Only fall back to closing the
                // tab when it's a single pane — never silently destroy a multi-pane
                // split whose target the user can't see.
                let single = self
                    .tabs
                    .get(self.active)
                    .is_some_and(|tab| tab.pane.leaves().len() <= 1);
                if single {
                    self.close_tab(self.active, window, cx);
                }
            }
            CloseOutcome::Collapsed => {
                if let Some(leaf) = &focused {
                    crate::terminal::RemoteTerminal::kill_pane(leaf.read(cx).pane_id);
                }
                self.focus_active(window, cx);
                self.save_session(cx);
                cx.notify();
            }
        }
    }

    /// Close the pane whose shell just exited on its own (`ChildExited` from
    /// the view — `exit`, Ctrl-D, a crashed shell): collapse its split, or
    /// close its tab when it was the only pane. Unlike `close_pane` this
    /// targets the *emitting* leaf, not the focused one — the exit can happen
    /// in a background tab. The daemon pane is killed even though its child is
    /// already dead: the daemon still lists it for reattach, and killing is
    /// what drops it from the session.
    fn on_child_exited(
        &mut self,
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = view.entity_id();
        let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.pane.leaves().iter().any(|l| l.entity_id() == id))
        else {
            return; // already closed (e.g. by the user racing the exit)
        };
        match self.tabs[index].pane.close_leaf(&view) {
            // The exited pane was the tab's only leaf: close the whole tab
            // (which snapshots it for reopen and kills its daemon panes).
            CloseOutcome::RemoveSelf => self.close_tab(index, window, cx),
            // Unreachable — containment was just checked — but never close a
            // tab we failed to locate the leaf in.
            CloseOutcome::NotFound => {}
            CloseOutcome::Collapsed => {
                crate::terminal::RemoteTerminal::kill_pane(view.read(cx).pane_id);
                if index == self.active {
                    self.maximized = None;
                    self.focus_active(window, cx);
                }
                self.save_session(cx);
                cx.notify();
            }
        }
    }

    /// Cycle focus among the panes of the active tab.
    fn cycle_pane(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        // `leaves()` returns owned clones, so the immutable borrow of `self.tabs`
        // ends here — letting us mutate `self.maximized` just below.
        let leaves = match self.tabs.get(self.active) {
            Some(tab) => tab.pane.leaves(),
            None => return,
        };
        if leaves.len() < 2 {
            return;
        }
        self.maximized = None;
        let current = leaves
            .iter()
            .position(|l| l.read(cx).focus_handle.contains_focused(window, cx))
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % leaves.len()
        } else {
            (current + leaves.len() - 1) % leaves.len()
        };
        let leaf = leaves[next].clone();
        self.focus_leaf(&leaf, window, cx);
        cx.notify();
    }

    /// Move focus to the pane adjacent to the focused one in `dir` (tmux
    /// directional focus). A no-op when there's no neighbor that way.
    fn focus_pane_dir(&mut self, dir: Dir, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.pane.neighbor_in_dir(dir, window, cx))
        else {
            return;
        };
        self.maximized = None;
        self.focus_leaf(&target, window, cx);
        cx.notify();
    }

    /// Grow/shrink the focused pane along `dir` by one step, adjusting its
    /// nearest matching-axis split. Persists the new layout. A no-op when no
    /// split matches (e.g. a single-pane tab, or no divider on that axis).
    fn resize_pane(&mut self, dir: Dir, window: &mut Window, cx: &mut Context<Self>) {
        let changed = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.pane.resize_focused_pane(dir, RESIZE_STEP, window, cx));
        if changed {
            self.save_session(cx);
            cx.notify();
        }
    }

    /// Swap the focused pane with its next / previous sibling in leaf order
    /// (tmux `prefix }` / `prefix {`). The terminals trade tree positions;
    /// focus rides along with the moved terminal. Needs at least two panes.
    fn swap_pane(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (from, len) = match self.tabs.get(self.active) {
            Some(tab) => (tab.pane.focused_index(window, cx), tab.pane.leaves().len()),
            None => return,
        };
        if len < 2 {
            return;
        }
        let from = from.unwrap_or(0);
        let to = if forward {
            (from + 1) % len
        } else {
            (from + len - 1) % len
        };
        if let Some(tab) = self.tabs.get_mut(self.active) {
            if tab.pane.swap_leaf_indices(from, to) {
                self.maximized = None;
                self.save_session(cx);
                cx.notify();
            }
        }
    }

    /// Switch to the next / previous tab, wrapping around (tmux `prefix n/p`).
    /// A no-op with fewer than two tabs.
    fn cycle_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.tabs.len();
        if n < 2 {
            return;
        }
        let next = if forward {
            (self.active + 1) % n
        } else {
            (self.active + n - 1) % n
        };
        self.activate(next, window, cx);
    }

    pub(crate) fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.tabs.len() && index != self.active {
            self.maximized = None;
            self.active = index;
            self.focus_active(window, cx);
            self.save_session(cx);
            cx.notify();
        }
    }

    /// Toggle maximize on the active tab's focused pane (Cmd+Shift+Enter). When a
    /// pane is maximized the tab renders only that leaf full-window; toggling again
    /// (or any structural change) restores the split layout. A no-op when the
    /// active tab has a single pane (nothing to maximize).
    fn toggle_maximize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.maximized.is_some() {
            self.maximized = None;
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.pane.leaves().len() < 2 {
            return;
        }
        let leaf = tab.pane.focused_or_first(window, cx);
        if let Some(leaf) = leaf {
            let handle = leaf.read(cx).focus_handle.clone();
            self.maximized = Some(leaf);
            window.focus(&handle, cx);
            cx.notify();
        }
    }

    pub(crate) fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // Closing the last tab is allowed: zero tabs is the home page (see
        // `ui::home`), and `focus_active`/`render` both handle it.
        if index >= self.tabs.len() {
            return;
        }
        self.maximized = None;
        // A rename in progress stores a fixed tab index; removing a tab shifts
        // indices and would let the pending edit commit onto the wrong tab. Drop it.
        self.renaming = None;
        // Snapshot the tab (layout + each pane's current cwd + name) onto the
        // recently-closed stack so Cmd+Shift+T can bring it back. The settings
        // tab is ephemeral, so it is never snapshotted or reopened this way.
        if !self.tabs[index].is_settings() {
            let snapshot = tab_to_session(&self.tabs[index], cx);
            self.closed.push(snapshot);
            if self.closed.len() > MAX_CLOSED_TABS {
                self.closed.remove(0);
            }
            // Explicitly closing a tab kills its daemon panes (matching the old
            // in-process behavior: closing ends the shells). This is distinct from
            // *quitting* the app, where panes are detached and kept alive so the
            // next launch can re-attach. Reopen-closed-tab then spawns fresh in the
            // saved cwd, just like before the daemon split.
            for leaf in self.tabs[index].pane.leaves() {
                crate::terminal::RemoteTerminal::kill_pane(leaf.read(cx).pane_id);
            }
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            // Home page: keep `active` at a stable 0 (every access goes through
            // `tabs.get`, which yields None until a tab exists again).
            self.active = 0;
        } else if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if index < self.active {
            self.active -= 1;
        }
        self.focus_active(window, cx);
        self.save_session(cx);
        cx.notify();
    }

    /// Reorder tabs: move the tab at `from` to position `to` (drag-and-drop).
    /// Keeps the same tab active across the move and re-persists the session.
    pub(crate) fn move_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from == to || from >= self.tabs.len() || to >= self.tabs.len() {
            return;
        }
        // Reordering shifts indices; a pending rename keyed on a fixed index would
        // commit onto the wrong tab. Drop it.
        self.renaming = None;
        let was_active = self.active;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        // Re-derive the active index so the same logical tab stays selected:
        // removal shifts indices after `from` left, insertion shifts indices at
        // or after `to` right.
        self.active = if was_active == from {
            to
        } else {
            let mut a = was_active;
            if from < a {
                a -= 1;
            }
            if to <= a {
                a += 1;
            }
            a
        };
        self.save_session(cx);
        cx.notify();
    }

    /// Begin an inline rename of the tab at `index`: spawn a focused text input
    /// pre-filled with the current label, committing on Enter or blur.
    pub(crate) fn start_rename(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The settings tab has a fixed label and isn't user-renamable.
        if self.tabs.get(index).is_none_or(Tab::is_settings) {
            return;
        }
        let current = self.tab_label(&self.tabs[index], index, cx);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current));
        input.update(cx, |state, cx| state.focus(window, cx));
        let subs = vec![cx.subscribe_in(
            &input,
            window,
            |this, _input, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } | InputEvent::Blur => this.commit_rename(window, cx),
                _ => {}
            },
        )];
        self.renaming = Some(Renaming {
            index,
            input,
            _subs: subs,
        });
        cx.notify();
    }

    /// Commit the in-progress rename: a non-empty value becomes the tab's custom
    /// name; an empty value clears it (reverting to the title-derived label).
    /// Taking `renaming` first makes the focus change below re-entrancy-safe (the
    /// input's resulting Blur finds no active rename and returns).
    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        let value = renaming.input.read(cx).value().trim().to_string();
        if let Some(tab) = self.tabs.get_mut(renaming.index) {
            tab.name = if value.is_empty() { None } else { Some(value) };
        }
        self.save_session(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    // ----- Command palette -------------------------------------------------

    /// Build the full command catalog: the static commands plus one
    /// "Switch to Tab: …" entry per open tab (label matches the tab strip).
    fn palette_commands(&self, cx: &App) -> Vec<Command> {
        let mut commands = Command::base_commands();
        let profiles = ssh_config::discover_profiles();
        if !profiles.is_empty() {
            commands.push(Command {
                title: "SSH Profiles…".to_string(),
                kind: CommandKind::OpenSshProfilePicker(profiles),
            });
        }
        for (i, tab) in self.tabs.iter().enumerate() {
            // Skip the active tab — "switch to the tab you're already on" is a
            // no-op that only pads the list.
            if i == self.active {
                continue;
            }
            let label = self.tab_label(tab, i, cx);
            commands.push(Command {
                title: format!("Switch to Tab: {label}"),
                kind: CommandKind::ActivateTab(i),
            });
        }
        commands
    }

    /// Open the palette if closed, or close it if already open (Cmd+P toggles).
    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        // Build the catalog and hand it to a fresh palette view; it owns the
        // search input, filtering and keyboard nav, and emits a `PaletteEvent`
        // when the user confirms or dismisses.
        let commands = self.palette_commands(cx);
        let view = cx.new(|cx| PaletteView::new(commands, window, cx));
        self.palette_sub = Some(cx.subscribe_in(&view, window, Self::on_palette_event));
        self.palette = Some(view);
        cx.notify();
    }

    /// Run the confirmed command (or just close on dismiss) for the open palette.
    fn on_palette_event(
        &mut self,
        _view: &Entity<PaletteView>,
        ev: &PaletteEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            PaletteEvent::Confirm(kind) => {
                let kind = kind.clone();
                self.close_palette(window, cx);
                self.run_command(kind, window, cx);
            }
            PaletteEvent::Dismiss => self.close_palette(window, cx),
        }
    }

    /// Close the palette and hand focus back to the active terminal.
    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        self.palette_sub = None;
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Run a palette command by dispatching to the matching tab/pane operation.
    fn run_command(&mut self, kind: CommandKind, window: &mut Window, cx: &mut Context<Self>) {
        use CommandKind::*;
        match kind {
            NewTab => self.new_tab(window, cx),
            SplitRight => self.split(Axis::Horizontal, window, cx),
            SplitDown => self.split(Axis::Vertical, window, cx),
            ClosePane => self.close_pane(window, cx),
            NextPane => self.cycle_pane(true, window, cx),
            PrevPane => self.cycle_pane(false, window, cx),
            FocusPaneLeft => self.focus_pane_dir(Dir::Left, window, cx),
            FocusPaneRight => self.focus_pane_dir(Dir::Right, window, cx),
            FocusPaneUp => self.focus_pane_dir(Dir::Up, window, cx),
            FocusPaneDown => self.focus_pane_dir(Dir::Down, window, cx),
            ResizePaneLeft => self.resize_pane(Dir::Left, window, cx),
            ResizePaneRight => self.resize_pane(Dir::Right, window, cx),
            ResizePaneUp => self.resize_pane(Dir::Up, window, cx),
            ResizePaneDown => self.resize_pane(Dir::Down, window, cx),
            SwapPaneNext => self.swap_pane(true, window, cx),
            SwapPanePrev => self.swap_pane(false, window, cx),
            NextTab => self.cycle_tab(true, window, cx),
            PrevTab => self.cycle_tab(false, window, cx),
            ToggleMaximizePane => self.toggle_maximize(window, cx),
            ToggleFullscreen => window.toggle_fullscreen(),
            ResetFontSize => self.reset_font_size(cx),
            FindInTerminal => {
                // Open the search bar on the pane focus just returned to (the
                // palette closed before we got here, restoring terminal focus).
                if let Some(leaf) = self
                    .tabs
                    .get(self.active)
                    .and_then(|t| t.pane.focused_or_first(window, cx))
                {
                    leaf.update(cx, |view, cx| view.open_search(window, cx));
                }
            }
            ClearTerminal => {
                // Same focus story as FindInTerminal: act on the pane the closing
                // palette just handed focus back to.
                if let Some(leaf) = self
                    .tabs
                    .get(self.active)
                    .and_then(|t| t.pane.focused_or_first(window, cx))
                {
                    leaf.update(cx, |view, cx| view.clear_scrollback(cx));
                }
            }
            ReopenClosedTab => self.reopen_closed_tab(window, cx),
            OpenSettings => self.toggle_settings(window, cx),
            RestartDaemon => self.restart_daemon(window, cx),
            SetTheme(i) => {
                if let Some(id) = crate::ui::presets::all(cx).get(i).map(|t| t.id.clone()) {
                    self.set_preset(&id, window, cx);
                }
            }
            OpenSshProfile(profile) => {
                self.open_managed_ssh_spec(
                    SshSpec {
                        target: profile.alias,
                        args: Vec::new(),
                    },
                    window,
                    cx,
                );
            }
            OpenSshConnect(input) => {
                if let Ok(ssh) = parse_ssh_connect_input(&input) {
                    self.open_managed_ssh_spec(ssh, window, cx);
                }
            }
            // Handled inside `PaletteView` (opens a sub-list); these never emit a
            // `Confirm` for this variant, so they never reach here.
            OpenThemePicker | OpenSshConnectInput | OpenSshProfilePicker(_) => {}
            ActivateTab(i) => self.activate(i, window, cx),
        }
    }

    // ----- Settings tab (Cmd+,) -------------------------------------------

    /// Index of the dedicated settings tab, if one is open.
    fn settings_tab_index(&self) -> Option<usize> {
        self.tabs.iter().position(Tab::is_settings)
    }

    /// Open the settings tab (Cmd+,). If one already exists, just activate it;
    /// otherwise assemble a new tab from the per-widget builders below (each
    /// pre-filled from config, with its subscriptions pushed onto `subs`) and
    /// focus it.
    fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.settings_tab_index() {
            self.activate(index, window, cx);
            return;
        }
        let focus_handle = cx.focus_handle();
        let mut subs = Vec::new();
        let (font_select, font_bold_select, font_italic_select) =
            self.build_font_selects(&mut subs, window, cx);
        let (shell_program_input, shell_args_input, wd_path_input) =
            self.build_shell_inputs(&mut subs, window, cx);
        let scroll_slider = self.build_scroll_slider(&mut subs, window, cx);
        // Live filter for the theme picker panel; each keystroke re-renders the
        // (already-cheap) list so results narrow as you type.
        let theme_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search themes…"));
        subs.push(
            cx.subscribe_in(&theme_search, window, |_this, _i, ev, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        );
        self.maximized = None;
        self.tabs.push(Tab {
            pane: Pane::Empty,
            name: Some("Settings".to_string()),
            settings: Some(SettingsState {
                focus_handle: focus_handle.clone(),
                section: SettingsSection::Appearance,
                font_select,
                font_bold_select,
                font_italic_select,
                shell_program_input,
                shell_args_input,
                wd_path_input,
                scroll_slider,
                theme_editor: None,
                theme_panel_open: false,
                theme_search,
                recording: None,
                rebinding_note: None,
                _subs: subs,
            }),
        });
        self.active = self.tabs.len() - 1;
        window.focus(&focus_handle, cx);
        // Build the color editor if we opened straight onto an editable theme.
        self.rebuild_theme_editor(window, cx);
        self.save_session(cx);
        cx.notify();
    }

    /// Primary / bold / italic font-family pickers, seeded from config.
    fn build_font_selects(
        &mut self,
        subs: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (
        Entity<SelectState<SearchableVec<String>>>,
        Entity<SelectState<SearchableVec<String>>>,
        Entity<SelectState<SearchableVec<String>>>,
    ) {
        let cfg = cx.global::<Config>();
        let family = cfg.font_family.clone();
        let font_bold = cfg.font_family_bold.clone();
        let font_italic = cfg.font_family_italic.clone();
        // Every font the OS reports is selectable — we don't get to decide
        // that for the user. The picker's dropdown just caps its own height
        // (see `menu_max_h` in settings.rs) so browsing the full list doesn't
        // dump it all on screen at once; search still reaches everything.
        let mut font_names = cx.text_system().all_font_names();
        if !font_names.contains(&family) {
            font_names.push(family.clone());
            font_names.sort_unstable();
        }
        let selected_font_index = font_names
            .iter()
            .position(|n| *n == family)
            .map(|row| IndexPath::default().row(row));
        let font_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(font_names.clone()),
                selected_font_index,
                window,
                cx,
            )
            .searchable(true)
        });
        // Bold / italic pickers share the font list but prepend a "Default" row
        // (the `FONT_DEFAULT_LABEL` sentinel) so the user can clear a distinct
        // face back to synthesized emphasis.
        let build_alt_font_select = |value: &Option<String>,
                                     names: &[String],
                                     window: &mut Window,
                                     cx: &mut Context<Self>| {
            let mut rows = Vec::with_capacity(names.len() + 1);
            rows.push(crate::ui::settings::FONT_DEFAULT_LABEL.to_string());
            rows.extend(names.iter().cloned());
            let selected = value
                .as_ref()
                .and_then(|v| rows.iter().position(|n| n == v))
                .unwrap_or(0);
            cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(rows),
                    Some(IndexPath::default().row(selected)),
                    window,
                    cx,
                )
                .searchable(true)
            })
        };
        let font_bold_select = build_alt_font_select(&font_bold, &font_names, window, cx);
        let font_italic_select = build_alt_font_select(&font_italic, &font_names, window, cx);
        subs.push(cx.subscribe_in(
            &font_select,
            window,
            |this, _select, ev: &SelectEvent<SearchableVec<String>>, _window, cx| {
                if let SelectEvent::Confirm(Some(family)) = ev {
                    this.commit_font_family(family.clone(), cx);
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &font_bold_select,
            window,
            |this, _s, ev: &SelectEvent<SearchableVec<String>>, _w, cx| {
                if let SelectEvent::Confirm(Some(name)) = ev {
                    this.commit_font_family_emphasis(true, name.clone(), cx);
                }
            },
        ));
        subs.push(cx.subscribe_in(
            &font_italic_select,
            window,
            |this, _s, ev: &SelectEvent<SearchableVec<String>>, _w, cx| {
                if let SelectEvent::Confirm(Some(name)) = ev {
                    this.commit_font_family_emphasis(false, name.clone(), cx);
                }
            },
        ));
        (font_select, font_bold_select, font_italic_select)
    }

    /// Shell program/args and working-directory inputs, committing on Enter/blur.
    fn build_shell_inputs(
        &mut self,
        subs: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<InputState>, Entity<InputState>, Entity<InputState>) {
        let cfg = cx.global::<Config>();
        // Pre-fill the shell inputs from config; an unset `shell` leaves them
        // empty so the placeholders advertise the platform default.
        let (shell_program, shell_args) = match &cfg.shell {
            Some(s) => (s.program.clone(), s.args.join(" ")),
            None => (String::new(), String::new()),
        };
        let wd_path = cfg.working_directory.path.clone();
        let platform_default = if cfg!(windows) {
            "PowerShell"
        } else {
            "login shell"
        };
        let shell_program_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(platform_default)
                .default_value(shell_program)
        });
        let shell_args_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("none")
                .default_value(shell_args)
        });
        let wd_path_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("/path/to/directory")
                .default_value(wd_path)
        });
        let commit_shell = |this: &mut Self, ev: &InputEvent, cx: &mut Context<Self>| {
            if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.commit_shell(cx);
            }
        };
        let commit_wd = |this: &mut Self, ev: &InputEvent, cx: &mut Context<Self>| {
            if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.commit_working_directory_path(cx);
            }
        };
        subs.push(
            cx.subscribe_in(&shell_program_input, window, move |this, _i, ev, _w, cx| {
                commit_shell(this, ev, cx)
            }),
        );
        subs.push(
            cx.subscribe_in(&shell_args_input, window, move |this, _i, ev, _w, cx| {
                commit_shell(this, ev, cx)
            }),
        );
        subs.push(
            cx.subscribe_in(&wd_path_input, window, move |this, _i, ev, _w, cx| {
                commit_wd(this, ev, cx)
            }),
        );
        (shell_program_input, shell_args_input, wd_path_input)
    }

    /// Mouse-scroll multiplier slider (0.5×–5×). Emits `Change` continuously as
    /// the user drags; each writes + persists the multiplier.
    fn build_scroll_slider(
        &mut self,
        subs: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SliderState> {
        let scroll_mult = cx.global::<Config>().mouse_scroll_multiplier;
        let scroll_slider = cx.new(|_| {
            SliderState::new()
                .min(0.5)
                .max(5.0)
                .step(0.25)
                .default_value(scroll_mult)
        });
        subs.push(cx.subscribe_in(
            &scroll_slider,
            window,
            |this, _s, ev: &SliderEvent, _w, cx| {
                if let SliderEvent::Change(v) = ev {
                    this.set_mouse_scroll_multiplier(v.start(), cx);
                }
            },
        ));
        scroll_slider
    }

    /// Close the settings tab (Esc inside the panel).
    pub(crate) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.settings_tab_index() {
            self.close_tab(index, window, cx);
        }
    }

    /// Apply the picked font family live to every terminal and persist it.
    fn commit_font_family(&mut self, family: String, cx: &mut Context<Self>) {
        self.font_family = family.clone();
        for tab in &self.tabs {
            for leaf in tab.pane.leaves() {
                let family = family.clone();
                leaf.update(cx, |v, cx| v.set_font_family(family, cx));
            }
        }
        let cfg = cx.global_mut::<Config>();
        cfg.font_family = family;
        cfg.save();
        cx.notify();
    }

    /// Apply a distinct bold or italic face (or clear it back to synthesized
    /// emphasis when the `FONT_DEFAULT_LABEL` sentinel is picked) live to every
    /// pane, and persist it. `bold == true` targets the bold face, else italic.
    fn commit_font_family_emphasis(&mut self, bold: bool, name: String, cx: &mut Context<Self>) {
        let family = (name != crate::ui::settings::FONT_DEFAULT_LABEL).then_some(name);
        for tab in &self.tabs {
            for leaf in tab.pane.leaves() {
                let family = family.clone();
                leaf.update(cx, |v, cx| {
                    if bold {
                        v.set_font_family_bold(family, cx);
                    } else {
                        v.set_font_family_italic(family, cx);
                    }
                });
            }
        }
        if bold {
            self.font_family_bold = family.clone();
        } else {
            self.font_family_italic = family.clone();
        }
        let cfg = cx.global_mut::<Config>();
        if bold {
            cfg.font_family_bold = family;
        } else {
            cfg.font_family_italic = family;
        }
        cfg.save();
        cx.notify();
    }

    /// Re-apply hot-reloaded config to every live pane. Wired to
    /// `observe_global::<Config>`, so an external edit to `config.json` — picked
    /// up by the watcher in `main.rs`, which swaps the `Config` global — flows to
    /// the on-screen terminals without a restart. This complements `apply_theme`
    /// (which already handles the color side) by covering the font knobs that
    /// live on `Tty7App`/the panes: size, line height, and family.
    ///
    /// Each field is diffed against the currently-applied value and skipped when
    /// unchanged. That keeps this a no-op for the much more frequent case where
    /// *our own* code mutated the global (every font setter and `set_preset`
    /// writes it), and — because we never write the global or `save()` from here
    /// — closes the save → watch → reload loop that would otherwise oscillate.
    fn reload_from_config(&mut self, cx: &mut Context<Self>) {
        let (font_size, line_height, font_family, font_features) = {
            let cfg = cx.global::<Config>();
            (
                cfg.font_size,
                cfg.line_height,
                cfg.font_family.clone(),
                cfg.font_features.clone(),
            )
        };
        if font_size != self.font_size {
            self.font_size = font_size;
            let px_size = px(font_size);
            for tab in &self.tabs {
                for leaf in tab.pane.leaves() {
                    leaf.update(cx, |v, cx| {
                        v.font_size = px_size;
                        cx.notify();
                    });
                }
            }
        }
        if line_height != self.line_height {
            self.line_height = line_height;
            for tab in &self.tabs {
                for leaf in tab.pane.leaves() {
                    leaf.update(cx, |v, cx| {
                        v.line_height_mul = line_height;
                        cx.notify();
                    });
                }
            }
        }
        if font_family != self.font_family {
            self.font_family = font_family.clone();
            for tab in &self.tabs {
                for leaf in tab.pane.leaves() {
                    let family = font_family.clone();
                    leaf.update(cx, |v, cx| v.set_font_family(family, cx));
                }
            }
        }
        if font_features != self.font_features {
            self.font_features = font_features.clone();
            for tab in &self.tabs {
                for leaf in tab.pane.leaves() {
                    let features = font_features.clone();
                    leaf.update(cx, |v, cx| v.set_font_features(features, cx));
                }
            }
        }
        let (bold, italic) = {
            let cfg = cx.global::<Config>();
            (cfg.font_family_bold.clone(), cfg.font_family_italic.clone())
        };
        if bold != self.font_family_bold {
            self.font_family_bold = bold.clone();
            for tab in &self.tabs {
                for leaf in tab.pane.leaves() {
                    let bold = bold.clone();
                    leaf.update(cx, |v, cx| v.set_font_family_bold(bold, cx));
                }
            }
        }
        if italic != self.font_family_italic {
            self.font_family_italic = italic.clone();
            for tab in &self.tabs {
                for leaf in tab.pane.leaves() {
                    let italic = italic.clone();
                    leaf.update(cx, |v, cx| v.set_font_family_italic(italic, cx));
                }
            }
        }
        cx.notify();
    }

    /// Persist the shell program + args from the settings inputs. An empty
    /// program clears the override (`shell: None`), so the daemon falls back to
    /// the platform default. Only newly spawned panes pick this up — the daemon
    /// reads `config.json` fresh on each PTY spawn — so running shells are
    /// untouched. There's nothing to apply live here; we just save.
    fn commit_shell(&mut self, cx: &mut Context<Self>) {
        let Some(settings) = self
            .settings_tab_index()
            .and_then(|i| self.tabs[i].settings.as_ref())
        else {
            return;
        };
        let program = settings
            .shell_program_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        let args: Vec<String> = settings
            .shell_args_input
            .read(cx)
            .value()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let shell = if program.is_empty() {
            None
        } else {
            Some(ShellConfig { program, args })
        };
        let cfg = cx.global_mut::<Config>();
        if cfg.shell == shell {
            return; // no change — avoid a redundant disk write on every Blur
        }
        cfg.shell = shell;
        cfg.save();
        cx.notify();
    }

    /// Change the working-directory strategy. Only affects newly spawned panes
    /// (the daemon reads `config.json` fresh per spawn), like the shell setting.
    pub(crate) fn set_working_directory_strategy(
        &mut self,
        strategy: crate::core::config::WdStrategy,
        cx: &mut Context<Self>,
    ) {
        let cfg = cx.global_mut::<Config>();
        if cfg.working_directory.strategy == strategy {
            return;
        }
        cfg.working_directory.strategy = strategy;
        cfg.save();
        cx.notify();
    }

    /// Persist the custom working-directory path from the settings input. Only
    /// used when the strategy is `Custom`, but stored regardless so switching back
    /// restores it.
    fn commit_working_directory_path(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self
            .settings_tab_index()
            .and_then(|i| self.tabs[i].settings.as_ref())
            .map(|s| s.wd_path_input.read(cx).value().trim().to_string())
        else {
            return;
        };
        let cfg = cx.global_mut::<Config>();
        if cfg.working_directory.path == path {
            return;
        }
        cfg.working_directory.path = path;
        cfg.save();
        cx.notify();
    }

    /// The active tab's settings state, if it is the settings tab.
    pub(crate) fn active_settings(&self) -> Option<&SettingsState> {
        self.tabs.get(self.active).and_then(|t| t.settings.as_ref())
    }

    pub(crate) fn active_settings_mut(&mut self) -> Option<&mut SettingsState> {
        self.tabs
            .get_mut(self.active)
            .and_then(|t| t.settings.as_mut())
    }

    fn active_ssh_pane(&self, window: &Window, cx: &App) -> Option<(u64, RemoteContext)> {
        let pane = self
            .tabs
            .get(self.active)?
            .pane
            .focused_or_first(window, cx)?;
        let pane = pane.read(cx);
        Some((pane.pane_id, pane.remote_context()?))
    }

    /// Select a sidebar section in the active settings tab (no-op elsewhere).
    pub(crate) fn select_settings_section(
        &mut self,
        target: SettingsSection,
        cx: &mut Context<Self>,
    ) {
        if let Some(s) = self
            .tabs
            .get_mut(self.active)
            .and_then(|t| t.settings.as_mut())
        {
            s.section = target;
            // Leaving the Keybindings page abandons any in-progress capture, so
            // the interceptor doesn't keep swallowing keys off-screen.
            s.recording = None;
        }
        cx.notify();
    }

    // ----- Keybindings editing (Settings → Keybindings) --------------------

    /// Begin capturing a new shortcut for `action`: install a keystroke
    /// interceptor that swallows the next keypress and records it, and stash it
    /// on the settings state so it stays active only while recording. Any prior
    /// capture is replaced.
    pub(crate) fn start_recording_key(
        &mut self,
        action: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The interceptor fires app-wide *before* keymap dispatch, so a chord
        // like ⌘T is captured here instead of opening a new tab. It runs until
        // the returned `Subscription` is dropped (capture done / Esc / cancel).
        let this = cx.weak_entity();
        let intercept = cx.intercept_keystrokes(move |ev, _window, cx| {
            let keystroke = ev.keystroke.clone();
            let _ = this.update(cx, |this, cx| this.on_record_key(&keystroke, cx));
            // Keep the key from also triggering an action / reaching a surface.
            cx.stop_propagation();
        });
        self.record_gen = self.record_gen.wrapping_add(1);
        if let Some(s) = self.active_settings_mut() {
            s.rebinding_note = None;
            s.recording = Some(Recording {
                action,
                chords: Vec::new(),
                _intercept: intercept,
            });
        }
        cx.notify();
    }

    /// Handle a keystroke captured during recording. Esc cancels. Backspace
    /// removes the last captured chord, or — with nothing captured yet — resets
    /// the action to its default. Any other key appends a chord and (re)starts
    /// the pause-to-commit timer, so single chords and sequences (e.g. the tmux
    /// preset's `ctrl-b x`) are recorded the same way.
    fn on_record_key(&mut self, keystroke: &gpui::Keystroke, cx: &mut Context<Self>) {
        let Some((action, has_chords)) = self
            .active_settings()
            .and_then(|s| s.recording.as_ref())
            .map(|r| (r.action.clone(), !r.chords.is_empty()))
        else {
            return;
        };
        match keystroke.key.as_str() {
            "escape" => {
                self.stop_recording(cx);
                return;
            }
            "backspace" | "delete" => {
                if has_chords {
                    // Edit the sequence: drop the last chord and keep capturing.
                    if let Some(r) = self
                        .active_settings_mut()
                        .and_then(|s| s.recording.as_mut())
                    {
                        r.chords.pop();
                    }
                    let still_has = self
                        .active_settings()
                        .and_then(|s| s.recording.as_ref())
                        .is_some_and(|r| !r.chords.is_empty());
                    if still_has {
                        self.schedule_recording_commit(cx);
                    } else {
                        // Nothing left to commit; wait for a fresh keypress.
                        self.record_gen = self.record_gen.wrapping_add(1);
                    }
                    cx.notify();
                } else {
                    self.stop_recording(cx);
                    self.reset_keybinding(action, cx);
                }
                return;
            }
            _ => {}
        }
        // A lone modifier press (⌘ held, no key yet) has nothing to bind — keep
        // waiting for a real key.
        let Some(spec) = crate::ui::keymap::spec_from_keystroke(keystroke) else {
            return;
        };
        if let Some(r) = self
            .active_settings_mut()
            .and_then(|s| s.recording.as_mut())
        {
            r.chords.push(spec);
        }
        self.schedule_recording_commit(cx);
        cx.notify();
    }

    /// (Re)arm the pause-to-commit timer: after a short quiet window with no new
    /// chord, the captured sequence is committed. Bumping `record_gen` first
    /// invalidates any earlier timer, so only the latest keypress's timer fires.
    fn schedule_recording_commit(&mut self, cx: &mut Context<Self>) {
        self.record_gen = self.record_gen.wrapping_add(1);
        let generation = self.record_gen;
        cx.spawn(async move |this, cx| {
            smol::Timer::after(std::time::Duration::from_millis(RECORD_COMMIT_DELAY_MS)).await;
            let _ = this.update(cx, |this, cx| {
                if this.record_gen == generation {
                    this.commit_recording(cx);
                }
            });
        })
        .detach();
    }

    /// Commit the captured chords (joined into a sequence spec) as the action's
    /// override. A no-op if capture ended or nothing was captured.
    fn commit_recording(&mut self, cx: &mut Context<Self>) {
        let Some((action, chords)) = self
            .active_settings()
            .and_then(|s| s.recording.as_ref())
            .filter(|r| !r.chords.is_empty())
            .map(|r| (r.action.clone(), r.chords.clone()))
        else {
            return;
        };
        self.stop_recording(cx);
        self.assign_keybinding(action, chords.join(" "), cx);
    }

    /// Drop the active capture (interceptor released, any pending commit timer
    /// invalidated) without changing anything.
    fn stop_recording(&mut self, cx: &mut Context<Self>) {
        self.record_gen = self.record_gen.wrapping_add(1);
        if let Some(s) = self.active_settings_mut() {
            s.recording = None;
        }
        cx.notify();
    }

    /// Assign `spec` to `action`. If another action already owns that keystroke,
    /// unbind it (last-writer-wins would otherwise be order-dependent) and note
    /// the takeover so the user can undo it with a reset.
    fn assign_keybinding(&mut self, action: String, spec: String, cx: &mut Context<Self>) {
        // Find the current owner of this exact keystroke, if it isn't `action`.
        let displaced = crate::ui::keymap::effective_bindings(cx)
            .into_iter()
            .find(|(a, k)| *k == spec && *a != action)
            .map(|(a, _)| a);
        let note = displaced.as_ref().map(|other| {
            format!(
                "{} took the shortcut from {}, which is now unset.",
                humanize_action(&action),
                humanize_action(other)
            )
        });
        self.update_config(cx, |cfg| {
            if let Some(other) = &displaced {
                // Explicit empty override = "unbound" (distinct from a reset,
                // which would restore that action's default and re-conflict).
                cfg.keybindings.insert(other.clone(), String::new());
            }
            cfg.keybindings.insert(action, spec);
        });
        crate::ui::keymap::rebind(cx);
        if let Some(s) = self.active_settings_mut() {
            s.rebinding_note = note;
        }
        cx.notify();
    }

    /// Reset one action to its built-in default (drop its override) and
    /// re-install the keymap.
    pub(crate) fn reset_keybinding(&mut self, action: String, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| {
            cfg.keybindings.remove(&action);
        });
        crate::ui::keymap::rebind(cx);
        if let Some(s) = self.active_settings_mut() {
            s.recording = None;
            s.rebinding_note = None;
        }
        cx.notify();
    }

    /// Clear every keybinding override, restoring the full default table.
    pub(crate) fn restore_default_keybindings(&mut self, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.keybindings.clear());
        crate::ui::keymap::rebind(cx);
        if let Some(s) = self.active_settings_mut() {
            s.recording = None;
            s.rebinding_note = None;
        }
        cx.notify();
    }

    /// Switch the keybinding preset ("default" / "tmux") and re-install the
    /// keymap so the change is live immediately.
    pub(crate) fn set_keybinding_preset(&mut self, preset: &str, cx: &mut Context<Self>) {
        let preset = preset.to_string();
        self.update_config(cx, |cfg| cfg.keybinding_preset = preset);
        crate::ui::keymap::rebind(cx);
        if let Some(s) = self.active_settings_mut() {
            s.recording = None;
            s.rebinding_note = None;
        }
        cx.notify();
    }

    /// Set the tmux preset's prefix chord (e.g. `ctrl-b` / `ctrl-a`) and
    /// re-install the keymap.
    pub(crate) fn set_keybinding_prefix(&mut self, prefix: &str, cx: &mut Context<Self>) {
        let prefix = prefix.to_string();
        self.update_config(cx, |cfg| cfg.prefix = prefix);
        crate::ui::keymap::rebind(cx);
        cx.notify();
    }

    /// Open `config.json` with the OS default handler (Settings → Keybindings).
    /// A fresh install may never have saved yet, so write the current config
    /// first — the button must not point at a missing file.
    // The "Open config file" button was temporarily pulled from the UI; keep the
    // handler around so re-enabling it is a one-line change in `settings.rs`.
    #[allow(dead_code)]
    pub(crate) fn open_config_file(&self, cx: &Context<Self>) {
        let Some(path) = crate::core::config::config_path("config.json") else {
            return;
        };
        if !path.exists() {
            cx.global::<Config>().save();
        }
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(windows) {
            "explorer"
        } else {
            "xdg-open"
        };
        if let Err(e) = std::process::Command::new(opener).arg(&path).spawn() {
            log::warn!("failed to open {}: {e}", path.display());
        }
    }

    /// Open the GitHub Releases page in the browser — the "Download" action of
    /// the Settings → About update prompt. Deliberately hand-off, not
    /// self-update: the newest build is one click away on the web. Delegates to
    /// `core::update` so the settings button and the update modal share it.
    pub(crate) fn open_releases_page(&self) {
        crate::core::update::open_releases_page();
    }
}

impl Render for Tty7App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let strip = self.tab_strip(window, cx);
        let active_ssh_pane = self.active_ssh_pane(window, cx);
        // Render the active tab's pane tree; show focus rings only when split.
        let body = match self.tabs.get(self.active) {
            // Zero tabs: the window's own face — the home page (see `ui::home`).
            None => self.render_home(cx).into_any_element(),
            // The settings tab renders its panel instead of a terminal pane.
            Some(tab) if tab.is_settings() => self.render_settings(cx).into_any_element(),
            Some(active_tab) => {
                // If a pane is maximized and it belongs to the active tab, render
                // just that leaf full-window; otherwise the normal split layout.
                let maximized = self.maximized.as_ref().filter(|leaf| {
                    active_tab
                        .pane
                        .leaves()
                        .iter()
                        .any(|l| l.entity_id() == leaf.entity_id())
                });
                match maximized {
                    Some(leaf) => div()
                        .size_full()
                        .overflow_hidden()
                        .child(leaf.clone())
                        .into_any_element(),
                    None => {
                        let show_focus = active_tab.pane.leaves().len() > 1;
                        active_tab.pane.render(show_focus, window, cx)
                    }
                }
            }
        };

        div()
            .id("tty7-root")
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.new_tab(window, cx)))
            .on_action(
                cx.listener(|this, _: &CloseActiveTab, window, cx| this.close_pane(window, cx)),
            )
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| {
                this.split(Axis::Horizontal, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SplitDown, window, cx| {
                    this.split(Axis::Vertical, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &FocusNextPane, window, cx| {
                    this.cycle_pane(true, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &FocusPrevPane, window, cx| {
                    this.cycle_pane(false, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &FocusPaneLeft, window, cx| {
                this.focus_pane_dir(Dir::Left, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPaneRight, window, cx| {
                this.focus_pane_dir(Dir::Right, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPaneUp, window, cx| {
                this.focus_pane_dir(Dir::Up, window, cx)
            }))
            .on_action(cx.listener(|this, _: &FocusPaneDown, window, cx| {
                this.focus_pane_dir(Dir::Down, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ResizePaneLeft, window, cx| {
                this.resize_pane(Dir::Left, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ResizePaneRight, window, cx| {
                this.resize_pane(Dir::Right, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ResizePaneUp, window, cx| {
                this.resize_pane(Dir::Up, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ResizePaneDown, window, cx| {
                this.resize_pane(Dir::Down, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SwapPaneNext, window, cx| this.swap_pane(true, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SwapPanePrev, window, cx| this.swap_pane(false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &NextTab, window, cx| this.cycle_tab(true, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PrevTab, window, cx| this.cycle_tab(false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab1, window, cx| this.activate(0, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab2, window, cx| this.activate(1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab3, window, cx| this.activate(2, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab4, window, cx| this.activate(3, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab5, window, cx| this.activate(4, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab6, window, cx| this.activate(5, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab7, window, cx| this.activate(6, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab8, window, cx| this.activate(7, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab9, window, cx| this.activate(8, window, cx)),
            )
            .on_action(cx.listener(|this, _: &IncreaseFontSize, _window, cx| {
                this.change_font_size(FONT_SIZE_STEP, cx)
            }))
            .on_action(cx.listener(|this, _: &DecreaseFontSize, _window, cx| {
                this.change_font_size(-FONT_SIZE_STEP, cx)
            }))
            .on_action(cx.listener(|this, _: &ResetFontSize, _window, cx| this.reset_font_size(cx)))
            .on_action(
                cx.listener(|this, _: &TogglePalette, window, cx| this.toggle_palette(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ReopenClosedTab, window, cx| {
                this.reopen_closed_tab(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleMaximizePane, window, cx| {
                this.toggle_maximize(window, cx)
            }))
            .on_action(
                cx.listener(|_, _: &ToggleFullscreen, window, _cx| window.toggle_fullscreen()),
            )
            .on_action(
                cx.listener(|this, _: &OpenSettings, window, cx| this.toggle_settings(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &RestartDaemon, window, cx| this.restart_daemon(window, cx)),
            )
            // Quit lives on the same element-tree action path as every other Cmd
            // shortcut above, so a focused terminal routes `cmd-q` here rather
            // than relying solely on the global handler (which the keystroke
            // doesn't reach while focus is deep in the terminal view).
            .on_action(cx.listener(|_, _: &Quit, _, cx| cx.quit()))
            .child(
                TitleBar::new()
                    // Taller than the stock 34px bar so the tabs read substantial
                    // and roomy instead of cramped. `.h(..)` lands
                    // in the component's `refine_style`, which is applied after its
                    // own `.h(TITLE_BAR_HEIGHT)`, so this override wins.
                    .h(px(40.))
                    .bg(cx.theme().transparent)
                    .border_color(cx.theme().transparent)
                    // The tab strip anchors left; the title bar keeps its right
                    // edge clear (before the traffic lights' mirror gap on macOS).
                    .child(strip),
            )
            .child(
                div()
                    .flex_1()
                    .relative()
                    .overflow_hidden()
                    .child(body)
                    .when_some(active_ssh_pane, |this, (pane_id, remote)| {
                        this.child(self.render_loopback_forward_overlay(pane_id, &remote, cx))
                    }),
            )
            // Command palette overlay, layered above everything when open.
            .when_some(self.palette.clone(), |this, palette| this.child(palette))
    }
}

/// Convert a live `Tab` (pane tree + name) into its serializable mirror.
fn tab_to_session(tab: &Tab, cx: &App) -> SessionTab {
    SessionTab {
        name: tab.name.clone(),
        pane: pane_to_session(&tab.pane, cx),
    }
}

/// Convert a live `Pane` tree into its serializable mirror, reading each
/// leaf's current cwd and each split's axis + ratio. Used when saving.
fn pane_to_session(pane: &Pane, cx: &App) -> SessionPane {
    match pane {
        Pane::Leaf(view) => {
            let view = view.read(cx);
            SessionPane::Leaf {
                cwd: view.cwd(),
                pane_id: Some(view.pane_id),
            }
        }
        Pane::Split {
            axis, a, b, ratio, ..
        } => SessionPane::Split {
            axis: match axis {
                Axis::Horizontal => SessionAxis::Horizontal,
                Axis::Vertical => SessionAxis::Vertical,
            },
            ratio: ratio.get(),
            a: Box::new(pane_to_session(a, cx)),
            b: Box::new(pane_to_session(b, cx)),
        },
        // A transient `Empty` should never be persisted; mirror it as a bare
        // leaf so restore still yields a usable terminal.
        Pane::Empty => SessionPane::Leaf {
            cwd: None,
            pane_id: None,
        },
    }
}

/// Set of daemon pane ids currently alive, used by `session_to_pane` to decide
/// per leaf whether to re-`attach` or `spawn`. Computed once per restore from the
/// daemon's `List`; empty (→ all-fresh) when the daemon is unreachable.
fn alive_panes() -> std::collections::HashSet<u64> {
    crate::terminal::RemoteTerminal::list_panes()
        .into_iter()
        .filter(|p| p.alive)
        .map(|p| p.pane_id)
        .collect()
}

/// Rebuild the tab list from a persisted `Session`, re-attaching to still-live
/// daemon panes where possible and spawning fresh shells otherwise. An absent or
/// empty session yields no tabs (the home page). Shared by first-launch restore
/// (`Tty7App::new`) and the daemon-restart rebuild (`restart_daemon`), so the two
/// stay in lockstep.
fn tabs_from_session(
    session: Option<Session>,
    font_size: f32,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> (Vec<Tab>, usize) {
    let Some(session) = session.filter(|s| !s.tabs.is_empty()) else {
        return (Vec::new(), 0);
    };
    // Ask the daemon once which panes are still alive, so leaves re-attach to
    // surviving shells instead of all spawning fresh.
    let alive = alive_panes();
    let mut tabs: Vec<Tab> = Vec::with_capacity(session.tabs.len());
    for st in &session.tabs {
        let pane = session_to_pane(&st.pane, &alive, font_size, window, cx);
        tabs.push(Tab {
            pane,
            name: st.name.clone(),
            settings: None,
        });
    }
    // Clamp the saved active index into the rebuilt range.
    let active = session.active.min(tabs.len() - 1);
    (tabs, active)
}

/// Rebuild a live `Pane` tree from a saved `SessionPane`. A leaf whose saved
/// `pane_id` is still alive in the daemon re-`attach`es (process + scrollback
/// intact); otherwise it spawns a fresh shell in the saved cwd. `alive` is the
/// daemon's current pane set, computed once by the caller.
fn session_to_pane(
    sp: &SessionPane,
    alive: &std::collections::HashSet<u64>,
    font_size: f32,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> Pane {
    match sp {
        SessionPane::Leaf { cwd, pane_id } => {
            // Only restore the pane id when the daemon confirms it's still live;
            // a stale id (daemon restarted, pane killed) falls back to a spawn.
            let restore = (*pane_id).filter(|id| alive.contains(id));
            // A shell pick isn't persisted in the session, so a stale pane that
            // must respawn comes back on the default shell.
            let view = new_terminal(font_size, cwd.clone(), restore, None, window, cx);
            Pane::leaf(view)
        }
        SessionPane::Split { axis, ratio, a, b } => {
            let axis = match axis {
                SessionAxis::Horizontal => Axis::Horizontal,
                SessionAxis::Vertical => Axis::Vertical,
            };
            let a = session_to_pane(a, alive, font_size, window, cx);
            let b = session_to_pane(b, alive, font_size, window, cx);
            Pane::split_node(axis, *ratio, a, b)
        }
    }
}

fn new_terminal(
    font_size: f32,
    working_directory: Option<std::path::PathBuf>,
    restore_pane: Option<u64>,
    shell: Option<ShellSpec>,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> Entity<TerminalView> {
    let view = cx.new(|cx| {
        let mut view = TerminalView::new(working_directory, restore_pane, shell, window, cx)
            .expect("failed to start terminal");
        // Inherit the current global font size so new panes match existing ones.
        view.font_size = px(font_size);
        view
    });
    // A pane whose shell exits on its own (`exit`, Ctrl-D, a crash) closes
    // itself, like every other terminal. This is the single place all panes
    // are built — new tab, split, session restore — so the subscription
    // covers them all; restore even cleans up panes that died while no
    // client was attached (the daemon replays their exit on reattach).
    cx.subscribe_in(&view, window, |app, view, _: &ChildExited, window, cx| {
        app.on_child_exited(view.clone(), window, cx);
    })
    .detach();
    view
}

pub(crate) fn parse_ssh_option_words(input: &str) -> Result<Vec<String>, ()> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            (None, c) => current.push(c),
        }
    }
    if quote.is_some() {
        return Err(());
    }
    if !current.is_empty() {
        words.push(current);
    }
    Ok(words)
}

pub(crate) fn parse_ssh_connect_input(input: &str) -> Result<SshSpec, ()> {
    let mut words = parse_ssh_option_words(input)?;
    if words.first().is_some_and(|word| word == "ssh") {
        words.remove(0);
    }
    let Some(target_ix) = ssh_connect_target_ix(&words) else {
        return Err(());
    };
    let target = words.remove(target_ix);
    if target.trim().is_empty() {
        return Err(());
    }
    let ssh = SshSpec {
        target,
        args: words,
    };
    ssh.validate().map_err(|_| ())?;
    Ok(ssh)
}

fn ssh_connect_target_ix(words: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < words.len() {
        let word = &words[i];
        if word == "--" {
            return None;
        }
        if !word.starts_with('-') {
            return Some(i);
        }
        if ssh_short_option_value_flag(word).is_some() {
            i += 1;
            if i >= words.len() {
                return None;
            }
        }
        i += 1;
    }
    None
}

fn ssh_short_option_value_flag(word: &str) -> Option<char> {
    let short = word.strip_prefix('-')?;
    if short.is_empty() || short.starts_with('-') {
        return None;
    }
    let mut chars = short.chars();
    let flag = chars.next()?;
    if ssh_option_takes_value(flag) && chars.as_str().is_empty() {
        Some(flag)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_ssh_connect_input, parse_ssh_option_words};

    #[test]
    fn parses_ssh_option_words_with_quotes() {
        assert_eq!(
            parse_ssh_option_words("-p 2222 -J 'jump host' -o \"User=dev\"").unwrap(),
            vec!["-p", "2222", "-J", "jump host", "-o", "User=dev"]
        );
    }

    #[test]
    fn rejects_unclosed_ssh_option_quote() {
        assert!(parse_ssh_option_words("-J 'jump").is_err());
    }

    #[test]
    fn parses_ssh_connect_input_with_optional_ssh_prefix() {
        assert_eq!(
            parse_ssh_connect_input("ssh dev -p 2222 -J 'jump host'")
                .unwrap()
                .target,
            "dev"
        );
        let parsed = parse_ssh_connect_input("dev -p 2222").unwrap();
        assert_eq!(parsed.target, "dev");
        assert_eq!(parsed.args, vec!["-p", "2222"]);
    }

    #[test]
    fn parses_ssh_connect_input_with_options_before_target() {
        let parsed = parse_ssh_connect_input("ssh -p 2222 -J 'jump host' dev").unwrap();
        assert_eq!(parsed.target, "dev");
        assert_eq!(parsed.args, vec!["-p", "2222", "-J", "jump host"]);
    }

    #[test]
    fn parses_ssh_connect_input_with_quoted_target() {
        let parsed = parse_ssh_connect_input("ssh -l dev 'host name'").unwrap();
        assert_eq!(parsed.target, "host name");
        assert_eq!(parsed.args, vec!["-l", "dev"]);
    }

    #[test]
    fn rejects_ssh_connect_input_without_target() {
        assert!(parse_ssh_connect_input("ssh -p 2222").is_err());
    }

    #[test]
    fn rejects_ssh_connect_input_with_remote_command() {
        assert!(parse_ssh_connect_input("ssh dev uptime").is_err());
        assert!(parse_ssh_connect_input("ssh -- dev").is_err());
    }
}

#[cfg(test)]
mod keybinding_gpui_tests {
    use crate::core::config::Config;
    use crate::core::session::Session;
    use crate::ui::app::Tty7App;
    use crate::ui::settings::SettingsSection;
    use gpui::{TestAppContext, VisualTestContext, WindowHandle};

    fn harness(cx: &mut TestAppContext) -> (WindowHandle<Tty7App>, VisualTestContext) {
        // The pause-to-commit is a real `smol::Timer` (off the deterministic
        // executor), so waiting on it parks the test thread.
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            crate::ui::keymap::init(cx);
        });
        let window =
            cx.add_window(|window, cx| Tty7App::with_session(Some(Session::default()), window, cx));
        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.background_executor.run_until_parked();
        let vcx = VisualTestContext::from_window(window.into(), cx);
        (window, vcx)
    }

    /// Open Settings → Keybindings and begin capturing `action`.
    fn begin_capture(window: &WindowHandle<Tty7App>, vcx: &mut VisualTestContext, action: &str) {
        let action = action.to_string();
        window
            .update(vcx, |app, window, cx| {
                app.toggle_settings(window, cx);
                app.select_settings_section(SettingsSection::Keybindings, cx);
                app.start_recording_key(action, window, cx);
            })
            .unwrap();
    }

    /// Poll (bounded) until `action` has the expected override in config — the
    /// commit fires on a real ~650ms timer.
    fn wait_for_binding(vcx: &mut VisualTestContext, action: &str, expected: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            vcx.background_executor.run_until_parked();
            let got = vcx.update(|_, cx| cx.global::<Config>().keybindings.get(action).cloned());
            if got.as_deref() == Some(expected) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "binding for {action} never became {expected:?} (last {got:?})"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    // End-to-end: open Settings → Keybindings, capture a shortcut for New Tab,
    // and confirm the recorded keystroke is normalized, persisted to config, and
    // the capture ends. This drives the real interceptor path installed by
    // `start_recording_key`, not just the pure helpers.
    #[gpui::test]
    fn recording_a_shortcut_writes_the_override_and_ends_capture(cx: &mut TestAppContext) {
        let (window, mut vcx) = harness(cx);
        begin_capture(&window, &mut vcx, "NewTab");
        // The platform-primary modifier normalizes to `secondary` on write.
        vcx.simulate_keystrokes("secondary-shift-n");
        wait_for_binding(&mut vcx, "NewTab", "secondary-shift-n");

        let recording = window
            .update(&mut vcx, |app, _, _| {
                app.active_settings().map(|s| s.recording.is_some())
            })
            .unwrap();
        assert_eq!(
            recording,
            Some(false),
            "capture should end after committing"
        );
    }

    // A two-chord sequence (the tmux-style `ctrl-b x`) records as one binding.
    #[gpui::test]
    fn recording_a_two_chord_sequence_writes_the_full_spec(cx: &mut TestAppContext) {
        let (window, mut vcx) = harness(cx);
        begin_capture(&window, &mut vcx, "CloseActiveTab");
        // Two chords in quick succession, then the pause commits the sequence.
        // `secondary-b` is used (not a bare `ctrl-b`) so the recorded spec is
        // identical on macOS and elsewhere — the primary modifier normalizes to
        // `secondary` either way.
        vcx.simulate_keystrokes("secondary-b");
        vcx.simulate_keystrokes("x");
        wait_for_binding(&mut vcx, "CloseActiveTab", "secondary-b x");
    }

    // Esc during capture cancels without touching config.
    #[gpui::test]
    fn escape_cancels_capture_without_writing(cx: &mut TestAppContext) {
        let (window, mut vcx) = harness(cx);
        window
            .update(&mut vcx, |app, window, cx| {
                app.toggle_settings(window, cx);
                app.select_settings_section(SettingsSection::Keybindings, cx);
                app.start_recording_key("NewTab".to_string(), window, cx);
            })
            .unwrap();
        vcx.simulate_keystrokes("escape");
        vcx.background_executor.run_until_parked();

        let stored = vcx.update(|_, cx| cx.global::<Config>().keybindings.contains_key("NewTab"));
        assert!(!stored, "Esc must not persist a binding");
        let recording = window
            .update(&mut vcx, |app, _, _| {
                app.active_settings().map(|s| s.recording.is_some())
            })
            .unwrap();
        assert_eq!(recording, Some(false));
    }
}

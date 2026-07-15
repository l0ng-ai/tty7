//! The window shell: a transparent unified title bar carrying the tab strip,
//! with the active terminal filling the rest. Owns all tabs (each its own PTY).

use gpui::{
    App, Axis, Context, Entity, Focusable, PromptLevel, Subscription, Window, div, prelude::*, px,
};
use gpui_component::color_picker::{ColorPickerEvent, ColorPickerState};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::select::{SearchableVec, SelectEvent, SelectState};
use gpui_component::slider::{SliderEvent, SliderState};
use gpui_component::{ActiveTheme as _, IndexPath, TitleBar, WindowExt as _};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use crate::core::actions::*;
use crate::core::config::{Config, NewTabPosition, ShellConfig, TabBarPosition};
use crate::core::session::{Session, SessionAxis, SessionPane, SessionTab};
use crate::core::shells::DetectedShell;
use crate::core::ssh_config;
use crate::daemon::protocol::{RemoteContext, ShellSpec, ssh_option_takes_value};
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

/// Height (px) of the unified title bar. Shared by `render` (the strip's height),
/// the settings overlay's nav-sidebar top zone, and the tab rail's top zone so
/// they all line up (and reach the very top of the window).
pub(crate) const TITLE_BAR_HEIGHT: f32 = 40.;

/// One tab: a split-pane tree plus an optional user-assigned name. Settings is
/// no longer a tab — it's a full-window overlay (`Tty7App::settings`), so every
/// tab is a real terminal tab.
pub struct Tab {
    /// The tab's split-pane tree (one or more terminals).
    pub pane: Pane,
    /// User-set custom name (via "Rename Tab"). `None` → derive the label from
    /// the focused terminal's title at render time.
    pub name: Option<String>,
    /// Entity id of the pane that last held focus in this tab. Recorded when we
    /// leave the tab (see `remember_active_pane`) and restored on return, so
    /// switching away and back keeps the active pane instead of jumping to the
    /// first leaf. `None` for a tab never left, or after its focused pane closed
    /// — both fall back to `first_leaf()`.
    last_focused: Option<gpui::EntityId>,
}

impl Tab {
    fn new(pane: Pane) -> Self {
        Self {
            pane,
            name: None,
            last_focused: None,
        }
    }

    /// The pane to focus when this tab becomes active: the last-focused leaf if
    /// it still exists, otherwise the first leaf.
    fn focus_target(&self) -> Option<Entity<TerminalView>> {
        match self.last_focused {
            Some(id) => self.pane.leaf_matching_or_first(|l| l.entity_id() == id),
            None => self.pane.first_leaf(),
        }
    }

    /// The title used to derive the tab label: the pane the tab is working in.
    /// Only the *active* tab has a live focused pane, so for an inactive tab
    /// (which holds no window focus) we fall back to the pane it last had
    /// focused (`focus_target`) rather than always its first leaf — otherwise a
    /// background tab's label would snap to its first pane. Without a `window`
    /// (e.g. the command palette) the same `focus_target` is the best we have.
    /// Empty when there's no terminal or no title yet.
    pub(crate) fn leaf_title(&self, window: Option<&Window>, cx: &App) -> String {
        let leaf = match window {
            Some(window) => self
                .pane
                .focused_leaf(window, cx)
                .or_else(|| self.focus_target()),
            None => self.focus_target(),
        };
        leaf.map(|l| l.read(cx).title.clone()).unwrap_or_default()
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
    /// The unified forwards list (Local/Remote/Dynamic, including auto localhost
    /// forwards) for the open native-SSH pane (WS4).
    pub(crate) managed: Vec<crate::daemon::protocol::ManagedForward>,
    /// Add-forward form state (native-SSH panes only).
    pub(crate) mf_kind: crate::daemon::protocol::SshForwardKind,
    pub(crate) mf_bind_host: Entity<InputState>,
    pub(crate) mf_bind_port: Entity<InputState>,
    pub(crate) mf_target_host: Entity<InputState>,
    pub(crate) mf_target_port: Entity<InputState>,
    pub(crate) mf_description: Entity<InputState>,
    /// When editing an existing forward, the id being edited — the form shows
    /// Save/Cancel and re-establishes the forward on save. `None` = adding.
    pub(crate) mf_editing: Option<u64>,
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
    /// Pane-contextual SFTP file panel (WS5), bound to a focused native-SSH pane.
    pub(crate) sftp_panel: crate::ui::sftp::SftpPanelState,
    /// Vertical tab sidebar width (px), held in a shared `Cell` so the resize
    /// drag's window-level mouse listener can mutate it without the entity handle
    /// (mirrors the split divider's `ratio`). Seeded from `Config::sidebar_width`
    /// and persisted back when a drag ends.
    pub(crate) sidebar_width: Rc<Cell<f32>>,
    /// Whether the sidebar's resize handle is currently held.
    pub(crate) sidebar_dragging: Rc<Cell<bool>>,
    /// Scroll handle for the sidebar's row list, so activating a tab scrolls its
    /// row into view.
    pub(crate) sidebar_scroll: gpui::ScrollHandle,
    /// Filter box in the sidebar's top control bar ("Search tabs…"); its text
    /// narrows the visible rows by fuzzy-ish substring match on the tab label.
    pub(crate) sidebar_search: Entity<InputState>,
    /// Re-renders the sidebar on each search keystroke so results narrow live.
    _sidebar_search_sub: Subscription,
    /// `Some` while the settings page is open. Settings is a full-window overlay
    /// (not a tab), so it covers the tab rail / title bar and never clutters the
    /// tab list. Holds all the settings widget state + its subscriptions.
    settings: Option<SettingsState>,
    /// In-pane native-SSH auth / host-key sheet state (WS3). Holds the active
    /// prompt (keyed to the pane that raised it), its input widgets, and
    /// dismissable banners. Empty when no prompt is pending.
    pub(crate) ssh_prompt: crate::ui::ssh_prompt::SshPromptState,
    /// In-pane "confirm close of a live SSH session" state (PRD FR-E3): the close
    /// action awaiting confirmation, or `None` when no prompt is up.
    pub(crate) ssh_close_confirm: Option<SshCloseKind>,
}

/// Which close action a live-SSH close-confirmation is gating (PRD FR-E3).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshCloseKind {
    /// Close the whole tab at this index.
    Tab(usize),
    /// Close the focused pane.
    Pane,
}

impl Tty7App {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Restore the previous session (tab/split layout + each pane's cwd),
        // unless the user turned restore off — then start fresh. `None` takes the
        // first-run path in `with_session`, spawning a single default terminal.
        let session = if cx.global::<Config>().restore_session {
            Session::load()
        } else {
            None
        };
        Self::with_session(session, window, cx)
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
        let sftp_panel = crate::ui::sftp::SftpPanelState::new(window, cx);
        // Managed-forward add-form inputs (native-SSH panes).
        let mf_bind_host = cx.new(|cx| InputState::new(window, cx).default_value("127.0.0.1"));
        let mf_bind_port = cx.new(|cx| InputState::new(window, cx).placeholder("8080"));
        let mf_target_host = cx.new(|cx| InputState::new(window, cx).placeholder("127.0.0.1"));
        let mf_target_port = cx.new(|cx| InputState::new(window, cx).placeholder("80"));
        let mf_description = cx.new(|cx| InputState::new(window, cx).placeholder("description"));
        let sidebar_width = cx.global::<Config>().sidebar_width;
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
        // Sidebar tab filter. Each keystroke re-renders the (cheap) row list so
        // results narrow as you type — the same live-filter wiring the theme
        // picker uses.
        let sidebar_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search tabs…"));
        let sidebar_search_sub =
            cx.subscribe_in(&sidebar_search, window, |_this, _i, ev, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            });
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
                managed: Vec::new(),
                mf_kind: crate::daemon::protocol::SshForwardKind::Local,
                mf_bind_host,
                mf_bind_port,
                mf_target_host,
                mf_target_port,
                mf_description,
                mf_editing: None,
            },
            sftp_panel,
            sidebar_width: Rc::new(Cell::new(sidebar_width)),
            sidebar_dragging: Rc::new(Cell::new(false)),
            sidebar_scroll: gpui::ScrollHandle::new(),
            sidebar_search,
            _sidebar_search_sub: sidebar_search_sub,
            settings: None,
            ssh_prompt: crate::ui::ssh_prompt::SshPromptState::new(cx),
            ssh_close_confirm: None,
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
        let tabs: Vec<SessionTab> = self
            .tabs
            .iter()
            .map(|tab| tab_to_session(tab, cx))
            .collect();
        // Zero tabs is a real state (the home page) and is persisted as such, so
        // the next launch comes back to it instead of a fresh shell.
        if tabs.is_empty() {
            Session::default().save();
            return;
        }
        let active = self.active.min(tabs.len() - 1);
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
        // Leaving the current tab for the reopened one; snapshot its focused
        // pane so switching back restores it (same as `activate`).
        self.remember_active_pane(window, cx);
        self.maximized = None;
        let insert_at = self.new_tab_insert_at(cx);
        self.tabs.insert(
            insert_at,
            Tab {
                pane,
                name: st.name,
                last_focused: None,
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
        if self.settings.is_none() {
            return;
        }
        let id = cx.global::<Config>().theme_preset.clone();
        let theme = crate::ui::presets::by_id(cx, &id);
        if !theme.editable() {
            if let Some(s) = self.settings.as_mut() {
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

        if let Some(s) = self.settings.as_mut() {
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
    pub(crate) fn update_config(
        &mut self,
        cx: &mut Context<Self>,
        mutate: impl FnOnce(&mut Config),
    ) {
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

    /// Global default for native-SSH host-key verification (WS3, FR-S4). A
    /// per-profile override still wins where set.
    pub(crate) fn set_verify_host_keys(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.verify_host_keys = on);
    }

    /// Global default for confirming before closing a live SSH session (FR-E3).
    /// A per-profile `warn_on_close` override still wins where set.
    pub(crate) fn set_ssh_warn_on_close(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.ssh_warn_on_close = on);
    }

    /// Refresh the managed (Local/Remote/Dynamic) forwards for `pane_id` (WS4).
    pub(crate) fn refresh_managed_forwards(&mut self, pane_id: u64, cx: &mut Context<Self>) {
        self.loopback_panel.managed = crate::terminal::RemoteTerminal::list_forwards(pane_id);
        cx.notify();
    }

    /// Pick the kind for the add-forward form (native-SSH panes).
    pub(crate) fn set_managed_forward_kind(
        &mut self,
        kind: crate::daemon::protocol::SshForwardKind,
        cx: &mut Context<Self>,
    ) {
        self.loopback_panel.mf_kind = kind;
        cx.notify();
    }

    /// Establish the add-form's managed forward on `pane_id`'s connection, then
    /// clear the form. A blank/invalid bind port is ignored; Dynamic forwards need
    /// no target.
    pub(crate) fn add_managed_forward(
        &mut self,
        pane_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::daemon::protocol::{SshForwardKind, SshForwardRule};
        let kind = self.loopback_panel.mf_kind;
        let bind_host = self
            .loopback_panel
            .mf_bind_host
            .read(cx)
            .value()
            .trim()
            .to_string();
        let bind_host = if bind_host.is_empty() {
            "127.0.0.1".to_string()
        } else {
            bind_host
        };
        let Ok(bind_port) = self
            .loopback_panel
            .mf_bind_port
            .read(cx)
            .value()
            .trim()
            .parse::<u16>()
        else {
            return;
        };
        let target_host = self
            .loopback_panel
            .mf_target_host
            .read(cx)
            .value()
            .trim()
            .to_string();
        let target_port = self
            .loopback_panel
            .mf_target_port
            .read(cx)
            .value()
            .trim()
            .parse::<u16>()
            .unwrap_or(0);
        // Local/Remote require a target; Dynamic (SOCKS) does not.
        if kind != SshForwardKind::Dynamic && (target_host.is_empty() || target_port == 0) {
            return;
        }
        let description = self
            .loopback_panel
            .mf_description
            .read(cx)
            .value()
            .trim()
            .to_string();
        let rule = SshForwardRule {
            kind,
            bind_host,
            bind_port,
            target_host,
            target_port,
            description: (!description.is_empty()).then_some(description),
        };
        // Editing an existing forward = re-establish it: drop the old one first so
        // its listener frees the (possibly reused) bind port before the new one binds.
        if let Some(old_id) = self.loopback_panel.mf_editing.take() {
            let _ = crate::terminal::RemoteTerminal::remove_forward(pane_id, old_id);
        }
        self.loopback_panel.managed = crate::terminal::RemoteTerminal::add_forward(pane_id, rule);
        // Reset the value-carrying fields; keep bind host default.
        for input in [
            &self.loopback_panel.mf_bind_port,
            &self.loopback_panel.mf_target_host,
            &self.loopback_panel.mf_target_port,
            &self.loopback_panel.mf_description,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }

    /// Load an existing forward's values into the add form for editing
    /// (VSCode-style: change the port/target, Save re-establishes it).
    pub(crate) fn edit_managed_forward(
        &mut self,
        forward: crate::daemon::protocol::ManagedForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loopback_panel.mf_kind = forward.kind;
        self.loopback_panel.mf_editing = Some(forward.id);
        let target_port = if forward.target_port == 0 {
            String::new()
        } else {
            forward.target_port.to_string()
        };
        let fields: [(&Entity<InputState>, String); 5] = [
            (&self.loopback_panel.mf_bind_host, forward.bind_host.clone()),
            (
                &self.loopback_panel.mf_bind_port,
                forward.bind_port.to_string(),
            ),
            (
                &self.loopback_panel.mf_target_host,
                forward.target_host.clone(),
            ),
            (&self.loopback_panel.mf_target_port, target_port),
            (
                &self.loopback_panel.mf_description,
                forward.description.clone().unwrap_or_default(),
            ),
        ];
        for (input, value) in fields {
            input.update(cx, |input, cx| input.set_value(&value, window, cx));
        }
        cx.notify();
    }

    /// Leave edit mode without saving; clear the form back to the add defaults.
    pub(crate) fn cancel_managed_forward_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loopback_panel.mf_editing = None;
        for input in [
            &self.loopback_panel.mf_bind_port,
            &self.loopback_panel.mf_target_host,
            &self.loopback_panel.mf_target_port,
            &self.loopback_panel.mf_description,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.loopback_panel
            .mf_bind_host
            .update(cx, |input, cx| input.set_value("127.0.0.1", window, cx));
        cx.notify();
    }

    /// Tear down one managed forward by id (native-SSH panes).
    pub(crate) fn remove_managed_forward(
        &mut self,
        pane_id: u64,
        forward_id: u64,
        cx: &mut Context<Self>,
    ) {
        self.loopback_panel.managed =
            crate::terminal::RemoteTerminal::remove_forward(pane_id, forward_id);
        cx.notify();
    }

    pub(crate) fn toggle_loopback_forward_panel(&mut self, pane_id: u64, cx: &mut Context<Self>) {
        let should_open = self.loopback_panel.open_pane_id != Some(pane_id);
        if should_open {
            self.loopback_panel.open_pane_id = Some(pane_id);
            self.refresh_managed_forwards(pane_id, cx);
        } else {
            self.loopback_panel.open_pane_id = None;
        }
        cx.notify();
    }

    pub(crate) fn close_loopback_forward_panel(&mut self, cx: &mut Context<Self>) {
        self.loopback_panel.open_pane_id = None;
        cx.notify();
    }

    /// Route a typed "SSH: Add Connection…" line to the native engine (PRD §3.1/
    /// §3.3). The input is parsed as best-effort into a transient profile — a
    /// `user@host[:port]` target plus the trivially-mappable flags (`-p`, `-i`,
    /// `-l`, `-J`, `-o User=`/`-o Port=`). A line that can't be parsed into a host
    /// surfaces a diagnosable inline notice rather than silently shelling out.
    fn open_typed_ssh_connect(&mut self, input: &str, window: &mut Window, cx: &mut Context<Self>) {
        match parse_ssh_connect_input(input) {
            Ok(parsed) => {
                // `ssh` semantics: a target naming a `~/.ssh/config` alias
                // resolves through it, with typed flags overriding the config's
                // values. (After parsing, a port of 22 is indistinguishable
                // from "not given", so an explicit `-p 22` can't override a
                // config port — the one caveat of this overlay.)
                let (profile, proxy_jump) =
                    match ssh_config::resolve_alias_to_profile(&parsed.profile.host) {
                        Some(resolved) => {
                            let mut p = resolved.profile;
                            if !parsed.profile.user.is_empty() {
                                p.user = parsed.profile.user;
                            }
                            if parsed.profile.port != 22 {
                                p.port = parsed.profile.port;
                            }
                            if !parsed.profile.identity_files.is_empty() {
                                p.identity_files = parsed.profile.identity_files;
                            }
                            (p, parsed.proxy_jump.or(resolved.proxy_jump))
                        }
                        None => (parsed.profile, parsed.proxy_jump),
                    };
                let verify = cx.global::<Config>().verify_host_keys;
                let spec = crate::ui::ssh_connect::native_spec_from_transient_profile(
                    &profile,
                    proxy_jump,
                    &crate::core::keychain::OsCredentialStore,
                    verify,
                    &crate::ui::ssh_connect::config_alias_resolver,
                );
                self.open_native_ssh_tab(Box::new(spec), window, cx);
            }
            Err(reason) => self.push_ssh_connect_error(reason, cx),
        }
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

    /// Set where the tab bar is rendered (Settings → Window & Tabs). Persists the
    /// choice; the layout re-derives from the `Config` global on the next render.
    pub(crate) fn set_tab_bar_position(&mut self, pos: TabBarPosition, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.tab_bar_position = pos);
    }

    /// `ToggleTabSidebar`: flip the tab bar between the horizontal title-bar strip
    /// (`Top`) and the vertical left sidebar (`Left`), persisting the choice.
    pub(crate) fn toggle_tab_sidebar(&mut self, cx: &mut Context<Self>) {
        let next = match cx.global::<Config>().tab_bar_position {
            TabBarPosition::Top => TabBarPosition::Left,
            TabBarPosition::Left => TabBarPosition::Top,
        };
        self.set_tab_bar_position(next, cx);
    }

    pub(crate) fn set_notify_mode(
        &mut self,
        mode: crate::core::config::NotifyMode,
        cx: &mut Context<Self>,
    ) {
        self.update_config(cx, |cfg| cfg.notify_on_command_finish = mode);
    }

    /// Set the "long command" floor (seconds) a foreground command must exceed
    /// to be eligible for a completion notification. Read live where the alert
    /// is posted, so nothing needs pushing to open panes.
    pub(crate) fn set_notify_threshold(&mut self, secs: u64, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.notify_threshold_secs = secs.clamp(1, 3600));
    }

    /// Switch how the terminal bell is signalled. Read live in each pane's bell
    /// handler, so there's nothing to push.
    pub(crate) fn set_bell_mode(
        &mut self,
        mode: crate::core::config::BellMode,
        cx: &mut Context<Self>,
    ) {
        self.update_config(cx, |cfg| cfg.bell = mode);
    }

    /// Toggle session restore. Takes effect on the next launch (this only
    /// persists the preference); the current window is untouched.
    pub(crate) fn set_restore_session(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.restore_session = on);
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

    /// Toggle whether mouse events reach full-screen apps. The gates are cached
    /// per view, so this pushes the new value into every open pane (like the
    /// font setters) in addition to persisting it.
    pub(crate) fn set_mouse_reporting(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.mouse_reporting = on);
        for tab in &self.tabs {
            for leaf in tab.pane.leaves() {
                leaf.update(cx, |v, cx| {
                    v.report_mouse = on;
                    cx.notify();
                });
            }
        }
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

    pub(crate) fn focus_active(&self, window: &mut Window, cx: &mut App) {
        // While the settings overlay is open it owns focus (so Esc-to-close and
        // keybinding capture keep working); tab operations behind it don't steal
        // it. `close_settings` refocuses the active terminal on the way out.
        if let Some(settings) = self.settings.as_ref() {
            window.focus(&settings.focus_handle, cx);
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            // No tabs → the home page is showing; keep something focused so
            // keystrokes stay on the window's dispatch path (⌘T etc. must still
            // reach the root action handlers).
            window.focus(&self.home_focus, cx);
            return;
        };
        if let Some(leaf) = tab.focus_target() {
            let handle = leaf.read(cx).focus_handle.clone();
            window.focus(&handle, cx);
        }
    }

    /// Snapshot which pane currently holds focus in the active tab into that
    /// tab's `last_focused`, so `focus_active` can restore it when we come back.
    /// Call this before any transition that moves focus off the active tab
    /// (switching tabs, opening a focus-stealing overlay).
    fn remember_active_pane(&mut self, window: &Window, cx: &App) {
        let active = self.active;
        if let Some(tab) = self.tabs.get_mut(active) {
            if let Some(leaf) = tab.pane.focused_leaf(window, cx) {
                tab.last_focused = Some(leaf.entity_id());
            }
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
        // Leaving the current tab for the new one; snapshot its focused pane
        // so switching back restores it (same as `activate`).
        self.remember_active_pane(window, cx);
        self.maximized = None;
        let insert_at = self.new_tab_insert_at(cx);
        self.tabs.insert(insert_at, Tab::new(Pane::leaf(tab)));
        self.active = insert_at;
        self.focus_active(window, cx);
        self.save_session(cx);
        cx.notify();
    }

    /// Open a new tab running a native (russh) SSH session for the resolved
    /// `spec` (PRD FR-C1). The caller (`ui::ssh_connect`) has already pulled any
    /// keychain secrets into `spec`. Mirrors `new_tab_with_shell` but for the
    /// native backend.
    pub(crate) fn open_native_ssh_tab(
        &mut self,
        spec: Box<crate::daemon::protocol::NativeSshSpec>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = self.tabs.get(self.active).and_then(|t| {
            t.pane
                .focused_or_first(window, cx)
                .and_then(|leaf| leaf.read(cx).cwd())
        });
        let view = match new_terminal_native(self.font_size, cwd, spec, window, cx) {
            Ok(view) => view,
            Err(e) => {
                log::error!("native SSH spawn failed: {e}");
                window.push_notification(format!("SSH connection failed: {e}"), cx);
                return;
            }
        };
        // Leaving the current tab for the new one; snapshot its focused pane
        // so switching back restores it (same as `activate`).
        self.remember_active_pane(window, cx);
        self.maximized = None;
        let insert_at = self.new_tab_insert_at(cx);
        self.tabs.insert(insert_at, Tab::new(Pane::leaf(view)));
        self.active = insert_at;
        self.focus_active(window, cx);
        self.save_session(cx);
        cx.notify();
    }

    /// Respawn a native SSH pane **in place** (same tab / split slot), replacing a
    /// dead pane's view with a fresh native connection for `spec` (PRD FR-E4). The
    /// daemon re-establishes the profile's preconfigured forwards on connect.
    pub(crate) fn respawn_native_ssh_in_place(
        &mut self,
        dead: &Entity<TerminalView>,
        spec: Box<crate::daemon::protocol::NativeSshSpec>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = dead.read(cx).cwd();
        let fresh = match new_terminal_native(self.font_size, cwd, spec, window, cx) {
            Ok(view) => view,
            Err(e) => {
                log::error!("native SSH respawn failed: {e}");
                window.push_notification(format!("SSH reconnect failed: {e}"), cx);
                return;
            }
        };
        // Swap the fresh leaf into the dead one's position across every tab.
        for tab in &mut self.tabs {
            if tab.pane.replace_leaf(dead, fresh.clone()) {
                break;
            }
        }
        self.maximized = None;
        self.focus_leaf(&fresh, window, cx);
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
        // Splitting a native-SSH pane opens another SSH pane on the same
        // connection rather than dropping back to a local shell. Re-resolve the
        // persisted (secret-free) spec from its saved profile so keychain
        // secrets are re-applied, mirroring the reconnect path.
        let ssh_spec = target.read(cx).ssh_spec();
        let new = if let Some(spec) = ssh_spec {
            let resolved = crate::ui::ssh_connect::resolve_persisted_ssh_spec(spec, cx);
            match new_terminal_native(self.font_size, cwd, resolved, window, cx) {
                Ok(view) => view,
                Err(e) => {
                    log::error!("native SSH split spawn failed: {e}");
                    window.push_notification(format!("SSH connection failed: {e}"), cx);
                    return;
                }
            }
        } else {
            let shell = target.read(cx).shell_spec();
            new_terminal(self.font_size, cwd, None, shell, window, cx)
        };
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
        // FR-E3: if the focused pane is a live SSH session flagged warn-on-close,
        // raise the in-pane confirm sheet instead of closing outright.
        if self.ssh_close_confirm.is_none() && self.focused_pane_is_warn_ssh(window, cx) {
            self.ssh_close_confirm = Some(SshCloseKind::Pane);
            cx.notify();
            return;
        }
        self.ssh_close_confirm = None;
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
        // A native-SSH pane lingers instead of closing (PRD FR-C2/E4): a failed
        // connect's diagnostic must stay readable, and a dropped session's pane
        // is the anchor for the in-pane reconnect (`restart_ssh_session`) —
        // auto-close would make both unreachable. Only local shells fall through
        // to the close below.
        if view.read(cx).ssh_disconnected() {
            cx.notify();
            return;
        }
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
            // Remember the pane we're leaving focused so returning to this tab
            // restores it instead of jumping to the first leaf.
            self.remember_active_pane(window, cx);
            self.maximized = None;
            self.active = index;
            // In sidebar mode, pull the newly active row into view (a no-op when
            // the strip is horizontal — the handle tracks no painted list then).
            self.sidebar_scroll.scroll_to_item(index);
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
        // FR-E3: confirm before closing a tab that holds a live warn-on-close SSH
        // session (unless this call is the confirmation itself).
        let already_confirming = self.ssh_close_confirm == Some(SshCloseKind::Tab(index));
        if !already_confirming && self.tab_has_warn_ssh(index, cx) {
            self.ssh_close_confirm = Some(SshCloseKind::Tab(index));
            cx.notify();
            return;
        }
        self.ssh_close_confirm = None;
        self.maximized = None;
        // A rename in progress stores a fixed tab index; removing a tab shifts
        // indices and would let the pending edit commit onto the wrong tab. Drop it.
        self.renaming = None;
        // Snapshot the tab (layout + each pane's current cwd + name) onto the
        // recently-closed stack so Cmd+Shift+T can bring it back.
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
        if self.tabs.get(index).is_none() {
            return;
        }
        let current = self.tab_label(&self.tabs[index], index, Some(&*window), cx);
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

        // Saved SSH profiles, ordered by frecency then name (PRD FR-P3). Each row
        // connects (natively) on Enter and edits on ⌘⏎ / →.
        let cfg = cx.global::<Config>();
        let now = crate::core::config::unix_now();
        let mut profiles: Vec<&crate::core::ssh_profile::SshProfile> =
            cfg.ssh_profiles.iter().collect();
        profiles.sort_by(|a, b| {
            let score = |p: &crate::core::ssh_profile::SshProfile| {
                cfg.ssh_profile_frecency
                    .get(&p.id)
                    .map(|u| u.score(now))
                    .unwrap_or(0.0)
            };
            score(b)
                .partial_cmp(&score(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        for p in profiles {
            let subtitle = crate::core::ssh_profile::to_connect_string(p);
            let title = if p.name.is_empty() {
                subtitle.clone()
            } else {
                p.name.clone()
            };
            commands.push(
                Command::new(
                    format!("SSH: {title}"),
                    CommandKind::ConnectSavedProfile(p.id),
                )
                .with_subtitle(subtitle),
            );
        }

        // Saved profiles are the palette's *only* SSH listing: `~/.ssh/config`
        // hosts appear here after Settings → SSH → "Import from ~/.ssh/config"
        // turns them into profiles, never as a parallel live-discovered source
        // (two lists of the same hosts with different behaviors confused more
        // than it helped). Typing an alias into "SSH: Add Connection…" still
        // resolves it against ssh_config on the spot.

        for (i, tab) in self.tabs.iter().enumerate() {
            // Skip the active tab — "switch to the tab you're already on" is a
            // no-op that only pads the list.
            if i == self.active {
                continue;
            }
            let label = self.tab_label(tab, i, None, cx);
            commands.push(Command::new(
                format!("Switch to Tab: {label}"),
                CommandKind::ActivateTab(i),
            ));
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
    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            ToggleTabSidebar => self.toggle_tab_sidebar(cx),
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
            ToggleSftp => self.toggle_sftp(window, cx),
            RestartSshSession => self.restart_ssh_session(window, cx),
            SetTheme(i) => {
                if let Some(id) = crate::ui::presets::all(cx).get(i).map(|t| t.id.clone()) {
                    self.set_preset(&id, window, cx);
                }
            }
            OpenSshConnect(input) => self.open_typed_ssh_connect(&input, window, cx),
            ConnectSavedProfile(id) => self.connect_ssh_profile(id, window, cx),
            EditSavedProfile(id) => self.open_ssh_profile_in_settings(id, window, cx),
            QuickConnect(target) => {
                if let Some(qc) = crate::core::ssh_profile::parse_quick_connect(&target) {
                    self.quick_connect(qc, window, cx);
                }
            }
            SaveQuickConnect(target) => self.open_ssh_profile_new_from_target(target, window, cx),
            OpenSshProfiles => self.open_settings_section(SettingsSection::Ssh, window, cx),
            // Handled inside `PaletteView` (opens a sub-list); these never emit a
            // `Confirm` for this variant, so they never reach here.
            OpenThemePicker | OpenSshConnectInput => {}
            ActivateTab(i) => self.activate(i, window, cx),
        }
    }

    // ----- Settings tab (Cmd+,) -------------------------------------------

    /// Toggle the settings overlay (Cmd+,). If it's already open, close it;
    /// otherwise assemble its widget state (each control pre-filled from config,
    /// with its subscriptions pushed onto `subs`) and focus the page.
    fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            self.close_settings(window, cx);
            return;
        }
        // Settings is about to steal focus; snapshot the active pane so closing
        // it lands back on the same terminal rather than the tab's first leaf.
        self.remember_active_pane(window, cx);
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
        // Live filter for the nav-header settings search; each keystroke re-renders
        // the (cheap) nav rail so the result list narrows as you type.
        let settings_search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search settings…"));
        subs.push(
            cx.subscribe_in(&settings_search, window, |this, _i, ev, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.autoselect_settings_search(cx);
                    cx.notify();
                }
            }),
        );

        self.settings = Some(SettingsState {
            focus_handle: focus_handle.clone(),
            section: SettingsSection::Appearance,
            search: settings_search,
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
            ssh_form: None,
            ssh_detail: crate::ui::settings::SshDetail::None,
            _subs: subs,
        });
        // Land the caret in the search box so Settings opens ready to type/filter
        // (a blinking cursor), rather than on the inert page root. Escape still
        // closes — the root's key handler is an ancestor of the focused input.
        let search_focus = self
            .settings
            .as_ref()
            .map(|s| s.search.read(cx).focus_handle(cx));
        match search_focus {
            Some(handle) => window.focus(&handle, cx),
            None => window.focus(&focus_handle, cx),
        }
        // Build the color editor if we opened straight onto an editable theme.
        self.rebuild_theme_editor(window, cx);
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

    /// Close the settings overlay (Esc inside the panel, or Cmd+, again),
    /// dropping its widget state and returning focus to the active terminal.
    pub(crate) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    /// Open Settings focused on `section`, opening the overlay if it's closed.
    /// Unlike `toggle_settings`, this never closes an already-open Settings — the
    /// entry points that jump to a specific section (e.g. SSH profiles) use it.
    pub(crate) fn open_settings_section(
        &mut self,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings.is_none() {
            self.toggle_settings(window, cx);
        }
        self.select_settings_section(section, cx);
    }

    /// Open Settings → SSH with `id`'s profile loaded into the inline edit form
    /// (the ⌘⏎ / Edit affordance on a saved profile).
    pub(crate) fn open_ssh_profile_in_settings(
        &mut self,
        id: uuid::Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings_section(SettingsSection::Ssh, window, cx);
        if let Some(profile) = cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .find(|p| p.id == id)
            .cloned()
        {
            self.ssh_form_load(&profile, window, cx);
        }
    }

    /// Open Settings → SSH with a new profile seeded from a QuickConnect target
    /// ("save as profile"), ready to edit and save.
    pub(crate) fn open_ssh_profile_new_from_target(
        &mut self,
        target: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings_section(SettingsSection::Ssh, window, cx);
        let mut profile = crate::core::ssh_profile::SshProfile::new(String::new());
        if let Some(qc) = crate::core::ssh_profile::parse_quick_connect(&target) {
            profile.port = qc.port_or_default();
            profile.host = qc.host;
            if let Some(user) = qc.user {
                profile.user = user;
            }
            if profile.name.is_empty() {
                profile.name = profile.host.clone();
            }
        }
        self.ssh_form_load(&profile, window, cx);
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
        // Keep the runtime sidebar width in step with the config (an external
        // edit to `config.json`, or our own drag-end persist which re-fires this).
        self.sidebar_width.set(cx.global::<Config>().sidebar_width);
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
        // Mouse-reporting is cached per view (the gates run without a `cx`), so a
        // hot-reload must push it into every open pane. Diffed per leaf so an
        // unrelated config edit doesn't churn panes that already agree.
        let report_mouse = cx.global::<Config>().mouse_reporting;
        for tab in &self.tabs {
            for leaf in tab.pane.leaves() {
                leaf.update(cx, |v, cx| {
                    if v.report_mouse != report_mouse {
                        v.report_mouse = report_mouse;
                        cx.notify();
                    }
                });
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
        let Some(settings) = self.active_settings() else {
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
            .active_settings()
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
    /// The open settings page's state, if the overlay is showing. The single
    /// accessor every settings widget/handler reads, so the rest of the settings
    /// code is agnostic to where the state lives.
    pub(crate) fn active_settings(&self) -> Option<&SettingsState> {
        self.settings.as_ref()
    }

    pub(crate) fn active_settings_mut(&mut self) -> Option<&mut SettingsState> {
        self.settings.as_mut()
    }

    /// The status-dot colour for a tab whose representative pane is an SSH
    /// session (PRD FR-E2): native panes are phase-coloured (connecting = warning,
    /// connected = accent, failed/disconnected = red); a foreground `ssh` typed
    /// into a shell gets a plain neutral dot. `None` for non-SSH tabs (no dot).
    pub(crate) fn tab_ssh_dot(&self, tab: &Tab, cx: &App) -> Option<gpui::Hsla> {
        use crate::daemon::protocol::SshPhase;
        let leaf = tab.pane.first_leaf()?;
        let v = leaf.read(cx);
        let theme = cx.theme();
        if let Some(phase) = v.ssh_phase() {
            // Native pane.
            let color = if v.ssh_disconnected() {
                theme.danger
            } else {
                match phase {
                    SshPhase::Connecting | SshPhase::Authenticating => theme.warning,
                    SshPhase::Connected => theme.accent,
                    SshPhase::Failed { .. } => theme.danger,
                }
            };
            Some(color)
        } else if v.remote_context().is_some() {
            // A foreground `ssh` typed into a shell: a plain neutral dot.
            Some(theme.muted_foreground)
        } else {
            None
        }
    }

    /// Whether `leaf` is a live, connected native-SSH pane whose effective
    /// warn-on-close is on (per-profile override, else the global toggle).
    fn leaf_is_warn_ssh(&self, leaf: &Entity<TerminalView>, cx: &App) -> bool {
        use crate::daemon::protocol::SshPhase;
        let v = leaf.read(cx);
        let connected = matches!(v.ssh_phase(), Some(SshPhase::Connected)) && !v.terminal.exited;
        if !connected {
            return false;
        }
        let cfg = cx.global::<Config>();
        let per_profile = v
            .ssh_spec()
            .and_then(|s| s.profile_id.clone())
            .and_then(|id| uuid::Uuid::parse_str(&id).ok())
            .and_then(|id| cfg.ssh_profiles.iter().find(|p| p.id == id))
            .and_then(|p| p.warn_on_close);
        per_profile.unwrap_or(cfg.ssh_warn_on_close)
    }

    /// Whether the tab at `index` holds any live warn-on-close SSH pane (FR-E3).
    pub(crate) fn tab_has_warn_ssh(&self, index: usize, cx: &App) -> bool {
        self.tabs
            .get(index)
            .map(|t| t.pane.leaves().iter().any(|l| self.leaf_is_warn_ssh(l, cx)))
            .unwrap_or(false)
    }

    /// Whether the focused pane is a live warn-on-close SSH pane (FR-E3).
    pub(crate) fn focused_pane_is_warn_ssh(&self, window: &Window, cx: &App) -> bool {
        self.tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
            .map(|l| self.leaf_is_warn_ssh(&l, cx))
            .unwrap_or(false)
    }

    /// Proceed with a pending SSH-close after confirmation (FR-E3).
    pub(crate) fn confirm_ssh_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.ssh_close_confirm {
            Some(SshCloseKind::Tab(i)) => self.close_tab(i, window, cx),
            Some(SshCloseKind::Pane) => self.close_pane(window, cx),
            None => {}
        }
    }

    /// Dismiss the SSH-close confirmation, leaving the session open (FR-E3).
    pub(crate) fn cancel_ssh_close(&mut self, cx: &mut Context<Self>) {
        self.ssh_close_confirm = None;
        cx.notify();
    }

    pub(crate) fn active_ssh_pane(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<(u64, RemoteContext)> {
        let pane = self
            .tabs
            .get(self.active)?
            .pane
            .focused_or_first(window, cx)?;
        let pane = pane.read(cx);
        Some((pane.pane_id, pane.remote_context()?))
    }

    /// The focused pane when it is a *connected native* SSH session — the gate for
    /// the pane's tunnel / SFTP action buttons (top-right of the terminal body).
    /// `None` for a foreground `ssh`, a still-connecting native pane, or a non-SSH
    /// pane, so those never grow the action buttons.
    pub(crate) fn active_connected_native_ssh_pane(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<(u64, RemoteContext)> {
        use crate::daemon::protocol::{RemoteKind, SshPhase};
        let (pane_id, remote) = self.active_ssh_pane(window, cx)?;
        if remote.kind != RemoteKind::NativeSsh {
            return None;
        }
        let leaf = self
            .tabs
            .get(self.active)?
            .pane
            .focused_or_first(window, cx)?;
        matches!(leaf.read(cx).ssh_phase(), Some(SshPhase::Connected)).then_some((pane_id, remote))
    }

    /// Select a sidebar section in the settings page (no-op when it's closed).
    pub(crate) fn select_settings_section(
        &mut self,
        target: SettingsSection,
        cx: &mut Context<Self>,
    ) {
        if let Some(s) = self.settings.as_mut() {
            s.section = target;
            // Leaving the Keybindings page abandons any in-progress capture, so
            // the interceptor doesn't keep swallowing keys off-screen.
            s.recording = None;
        }
        cx.notify();
    }

    /// Keep the settings selection on a section that has search hits: if the
    /// query changed and the current section no longer matches, jump to the
    /// best-matching one so the shown page always reflects the search. A section
    /// that still has matches is left alone, so the user's own click isn't yanked
    /// away as they keep typing.
    pub(crate) fn autoselect_settings_search(&mut self, cx: &mut Context<Self>) {
        let Some(settings) = self.settings.as_ref() else {
            return;
        };
        let query = settings.search.read(cx).value().trim().to_lowercase();
        if query.is_empty() {
            return;
        }
        if crate::ui::settings::section_match_count(settings.section, &query) > 0 {
            return;
        }
        if let Some(best) = crate::ui::settings::best_matching_section(&query) {
            self.select_settings_section(best, cx);
        }
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
        // Vertical-tab mode: the sidebar owns the tab list, so the title-bar strip
        // drops its chips (keeping only "+"/"⋯"). Gated on having tabs — the
        // zero-tab home page keeps the full-width horizontal layout, so an empty
        // rail never appears.
        let vertical = matches!(cx.global::<Config>().tab_bar_position, TabBarPosition::Left)
            && !self.tabs.is_empty();
        let strip = self.tab_strip(!vertical, window, cx);
        let sidebar = vertical.then(|| self.tab_sidebar(window, cx));
        // Gate the pane action buttons (tunnel / SFTP) + their panels to a
        // connected native-SSH pane; a foreground `ssh` or a still-connecting
        // session shows only the top-left status strip, no action buttons.
        let active_ssh_pane = self.active_connected_native_ssh_pane(window, cx);
        // The Cmd+F find bar pins to the same top-right slot as these action
        // buttons and, being deep inside the pane tree, paints *under* them
        // (gpui stacks by child order, and this overlay is a later sibling of
        // `body`). Give the find bar that slot: while it's open on the focused
        // pane, suppress the tunnel/SFTP icons so they don't bleed through.
        let search_open = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
            .map(|leaf| leaf.read(cx).search.is_some())
            .unwrap_or(false);
        // Native-SSH status strip / reconnect notice for the focused pane (E1/E4).
        let ssh_status = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
            .and_then(|leaf| self.render_ssh_status_strip(&leaf, cx));
        // Render the active tab's pane tree; show focus rings only when split.
        let body = match self.tabs.get(self.active) {
            // Zero tabs: the window's own face — the home page (see `ui::home`).
            None => self.render_home(cx).into_any_element(),
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

        // The title strip (a transparent unified title bar carrying `strip`) and
        // the terminal body area — shared by both layouts.
        let title_bar = TitleBar::new()
            // Taller than the stock 34px bar so the tabs read substantial and
            // roomy instead of cramped. `.h(..)` lands in the component's
            // `refine_style`, applied after its own `.h(TITLE_BAR_HEIGHT)`, so
            // this override wins.
            .h(px(TITLE_BAR_HEIGHT))
            .bg(cx.theme().transparent)
            .border_color(cx.theme().transparent)
            .child(strip);
        let body_area = div()
            .flex_1()
            .relative()
            .overflow_hidden()
            .child(body)
            // Pane-contextual tunnel / SFTP action buttons, pinned top-right of
            // the terminal area when the active pane is a connected native SSH
            // session (the tunnel button also drives the forwards panel).
            .when_some(active_ssh_pane, |this, (pane_id, remote)| {
                // Hide the top-right tunnel/SFTP icons while the find bar owns
                // that slot; the bottom-docked SFTP panel is unaffected.
                this.when(!search_open, |this| {
                    this.child(self.render_loopback_forward_overlay(pane_id, &remote, cx))
                })
                // Pane-contextual SFTP panel (WS5), docked right when open for
                // this (native-SSH) pane.
                .when_some(
                    self.render_sftp_overlay(pane_id, &remote, window, cx),
                    |this, panel| this.child(panel),
                )
            })
            // In-pane native-SSH auth / host-key sheet (WS3), shown over the pane
            // that raised the prompt.
            .when_some(self.render_ssh_prompt_overlay(window, cx), |this, el| {
                this.child(el)
            })
            // Native-SSH status strip / reconnect notice (E1/E4).
            .when_some(ssh_status, |this, el| this.child(el))
            // Live-SSH close-confirmation sheet (E3).
            .when_some(self.render_ssh_close_confirm_overlay(cx), |this, el| {
                this.child(el)
            });

        // The two layouts. Horizontal (default): a column of [title bar / body].
        // Vertical: the rail is a full-height *left column* that reaches the very
        // top of the window — the traffic lights sit on its surface — with the
        // title strip and terminal stacked in the right column. That way the rail
        // surface has no seam with the title bar and reads as one continuous
        // panel.
        let main_layout = match sidebar {
            Some(sidebar) => div()
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .flex_row()
                .child(sidebar)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(title_bar)
                        .child(body_area),
                )
                .into_any_element(),
            None => div()
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .flex_col()
                .child(title_bar)
                .child(body_area)
                .into_any_element(),
        };

        // Settings is a full-window overlay (not a tab): it covers the tab rail,
        // title strip, and terminal so it never crowds the tab list. `occlude`
        // blocks input to the elements behind it. It fills the window edge to
        // edge — its own nav sidebar reserves the title-bar zone internally (so
        // that rail reaches the top like the tab rail), rather than insetting the
        // whole page here.
        let settings_overlay = self.settings.is_some().then(|| {
            div()
                .absolute()
                .inset_0()
                .occlude()
                .bg(cx.theme().background)
                .child(self.render_settings(cx))
        });

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
                cx.listener(|this, _: &ToggleTabSidebar, _window, cx| this.toggle_tab_sidebar(cx)),
            )
            .on_action(
                cx.listener(|this, _: &OpenSettings, window, cx| this.toggle_settings(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &RestartDaemon, window, cx| this.restart_daemon(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleSftp, window, cx| this.toggle_sftp(window, cx)))
            // Quit lives on the same element-tree action path as every other Cmd
            // shortcut above, so a focused terminal routes `cmd-q` here rather
            // than relying solely on the global handler (which the keystroke
            // doesn't reach while focus is deep in the terminal view).
            .on_action(cx.listener(|_, _: &Quit, _, cx| cx.quit()))
            .on_action(cx.listener(|this, _: &OpenSshProfiles, window, cx| {
                this.open_settings_section(SettingsSection::Ssh, window, cx)
            }))
            .on_action(cx.listener(|this, _: &RestartSshSession, window, cx| {
                this.restart_ssh_session(window, cx)
            }))
            .child(main_layout)
            // Settings overlay, above the tabs/terminal when open.
            .when_some(settings_overlay, |this, overlay| this.child(overlay))
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
                // Persist the secret-free native-SSH spec so a *dead* pane can be
                // reconnected on restore (FR-E4/C2); `None` for local panes. A
                // live pane reattaches by `pane_id` and never needs this.
                ssh_spec: view.ssh_spec(),
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
            ssh_spec: None,
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
            last_focused: None,
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
        SessionPane::Leaf {
            cwd,
            pane_id,
            ssh_spec,
        } => {
            // Only restore the pane id when the daemon confirms it's still live;
            // a stale id (daemon restarted, pane killed) falls back to a spawn.
            let restore = (*pane_id).filter(|id| alive.contains(id));
            // A *dead* native-SSH leaf (spec persisted, pane no longer alive)
            // reconnects rather than dropping back to a local shell (FR-C2/E4):
            // re-resolve secrets from the profile when it names one, else reuse
            // the secret-free spec and let the auth sheets prompt.
            if restore.is_none() {
                if let Some(spec) = ssh_spec.clone() {
                    let resolved = crate::ui::ssh_connect::resolve_persisted_ssh_spec(spec, cx);
                    match new_terminal_native(font_size, cwd.clone(), resolved, window, cx) {
                        Ok(view) => return Pane::leaf(view),
                        // Keep restore alive: fall through to a local shell in
                        // this slot rather than aborting startup.
                        Err(e) => log::error!("restoring native SSH pane failed: {e}"),
                    }
                }
            }
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
    // Native-SSH auth/host-key prompts raised by this pane → in-pane sheet. Same
    // single build site as ChildExited, so every pane (new tab, split, restore)
    // is covered.
    cx.subscribe_in(
        &view,
        window,
        |app, view, _: &crate::terminal::view::AuthPromptReady, window, cx| {
            app.on_auth_prompt_ready(view.clone(), window, cx);
        },
    )
    .detach();
    view
}

/// Build a native (russh) SSH terminal view for `spec`, wiring the same
/// per-pane subscriptions (`ChildExited`, `AuthPromptReady`) as [`new_terminal`]
/// so it participates in auto-close and the in-pane auth sheets. Mirrors
/// `new_terminal` but takes the resolved connect spec instead of a shell.
/// Errors (daemon down/stale, spawn refused) are returned, never panicked —
/// callers surface them and keep the app alive.
pub(crate) fn new_terminal_native(
    font_size: f32,
    working_directory: Option<std::path::PathBuf>,
    spec: Box<crate::daemon::protocol::NativeSshSpec>,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> anyhow::Result<Entity<TerminalView>> {
    let parts = TerminalView::spawn_native_ssh_terminal(spec, working_directory)?;
    let view = cx.new(|cx| {
        let mut view = TerminalView::from_native_ssh_parts(parts, window, cx);
        view.font_size = px(font_size);
        view
    });
    cx.subscribe_in(&view, window, |app, view, _: &ChildExited, window, cx| {
        app.on_child_exited(view.clone(), window, cx);
    })
    .detach();
    cx.subscribe_in(
        &view,
        window,
        |app, view, _: &crate::terminal::view::AuthPromptReady, window, cx| {
            app.on_auth_prompt_ready(view.clone(), window, cx);
        },
    )
    .detach();
    Ok(view)
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

/// The data a typed "SSH: Add Connection…" line resolves to: a transient profile
/// plus the raw `ProxyJump` target (from `-J`), ready for
/// [`crate::ui::ssh_connect::native_spec_from_transient_profile`].
pub(crate) struct ParsedSshConnect {
    pub profile: crate::core::ssh_profile::SshProfile,
    pub proxy_jump: Option<String>,
}

/// Parse a typed connect line (`[ssh] [flags] user@host[:port]`) into native
/// connect data (PRD §3.1). Only the trivially-mappable flags are honored — `-p`,
/// `-l`, `-i` (repeatable), `-J`, and `-o User=`/`-o Port=`/`-o ProxyJump=`; other
/// options are ignored (best-effort). A remote command, a `--` separator, an
/// unbalanced quote, or a missing/invalid host is an `Err(reason)` surfaced as an
/// inline notice — never a silent shell-out. Returns the user-facing reason string.
pub(crate) fn parse_ssh_connect_input(input: &str) -> Result<ParsedSshConnect, String> {
    use crate::core::ssh_profile::{SshProfile, parse_quick_connect};

    let mut words = parse_ssh_option_words(input)
        .map_err(|_| "Unbalanced quotes in the SSH command".to_string())?;
    if words.first().is_some_and(|word| word == "ssh") {
        words.remove(0);
    }

    let mut target: Option<String> = None;
    let mut user: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut identities: Vec<String> = Vec::new();
    let mut jump: Option<String> = None;

    let mut i = 0;
    while i < words.len() {
        let word = words[i].clone();
        if word == "--" {
            return Err("Remote commands aren't supported here".to_string());
        }
        if let Some((flag, attached)) = ssh_short_flag(&word) {
            // Consume the value (attached `-p2222` form or the next word) when the
            // flag takes one.
            let value = if ssh_option_takes_value(flag) {
                if !attached.is_empty() {
                    attached
                } else {
                    i += 1;
                    match words.get(i) {
                        Some(v) => v.clone(),
                        None => return Err(format!("-{flag} needs a value")),
                    }
                }
            } else {
                String::new()
            };
            match flag {
                'p' => {
                    port = Some(
                        value
                            .parse::<u16>()
                            .ok()
                            .filter(|&p| p != 0)
                            .ok_or_else(|| format!("Invalid port \u{201c}{value}\u{201d}"))?,
                    )
                }
                'l' => user = Some(value),
                'i' => identities.push(value),
                'J' => jump = Some(value),
                'o' => apply_ssh_o_option(&value, &mut user, &mut port, &mut jump)?,
                // Any other flag (value already consumed if it took one) is ignored.
                _ => {}
            }
        } else if word.starts_with('-') {
            // A long option (`--foo`) or bare `-`: not something we map.
            return Err(format!("Unsupported option \u{201c}{word}\u{201d}"));
        } else if target.is_none() {
            target = Some(word);
        } else {
            return Err("Remote commands aren't supported here".to_string());
        }
        i += 1;
    }

    let target = target.ok_or_else(|| "Enter a host to connect to".to_string())?;
    let qc = parse_quick_connect(&target)
        .ok_or_else(|| format!("Can't parse host \u{201c}{target}\u{201d}"))?;

    let mut profile = SshProfile::new(qc.host.clone());
    profile.host = qc.host;
    // Explicit `-p` / `-o Port=` wins over a `:port` on the target, else default 22.
    profile.port = port.or(qc.port).unwrap_or(22);
    // Explicit `-l` / `-o User=` wins over `user@` on the target.
    if let Some(user) = user.or(qc.user) {
        profile.user = user;
    }
    profile.identity_files = identities;

    Ok(ParsedSshConnect {
        profile,
        proxy_jump: jump,
    })
}

/// Split a short-option word into `(flag, attached_value)` — `-p2222` → `('p',
/// "2222")`, `-J` → `('J', "")`. `None` for a non-option, `--`/long option, or a
/// bare `-`.
fn ssh_short_flag(word: &str) -> Option<(char, String)> {
    let rest = word.strip_prefix('-')?;
    if rest.is_empty() || rest.starts_with('-') {
        return None;
    }
    let mut chars = rest.chars();
    let flag = chars.next()?;
    Some((flag, chars.as_str().to_string()))
}

/// Apply the trivially-mappable `-o Name=Value` options (`User`/`Port`/
/// `ProxyJump`); anything else is ignored (best-effort).
fn apply_ssh_o_option(
    value: &str,
    user: &mut Option<String>,
    port: &mut Option<u16>,
    jump: &mut Option<String>,
) -> Result<(), String> {
    let Some((name, val)) = value.split_once('=') else {
        return Ok(());
    };
    match name.to_ascii_lowercase().as_str() {
        "user" => *user = Some(val.to_string()),
        "port" => {
            *port = Some(
                val.parse::<u16>()
                    .ok()
                    .filter(|&p| p != 0)
                    .ok_or_else(|| format!("Invalid port \u{201c}{val}\u{201d}"))?,
            )
        }
        "proxyjump" => *jump = Some(val.to_string()),
        _ => {}
    }
    Ok(())
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
    fn parses_typed_connect_into_native_profile() {
        // Bare `user@host:port` (optional `ssh` prefix) → transient profile.
        let p = parse_ssh_connect_input("ssh deploy@10.0.0.5:2222").unwrap();
        assert_eq!(p.profile.host, "10.0.0.5");
        assert_eq!(p.profile.user, "deploy");
        assert_eq!(p.profile.port, 2222);
        assert!(p.proxy_jump.is_none());
    }

    #[test]
    fn parses_typed_connect_flags_and_jump() {
        // Options before and after the target; `-p`/`-l`/`-i`/`-J` all map.
        let p =
            parse_ssh_connect_input("ssh -p 2222 -l dev -i ~/.ssh/id_ed25519 -J 'jump host' host")
                .unwrap();
        assert_eq!(p.profile.host, "host");
        assert_eq!(p.profile.user, "dev");
        assert_eq!(p.profile.port, 2222);
        assert_eq!(
            p.profile.identity_files,
            vec!["~/.ssh/id_ed25519".to_string()]
        );
        assert_eq!(p.proxy_jump.as_deref(), Some("jump host"));

        // Attached short-flag form (`-p2222`) and `-o User=`/`-o Port=`.
        let p = parse_ssh_connect_input("host -p2222 -o User=deploy -o Port=2200").unwrap();
        assert_eq!(p.profile.user, "deploy");
        // `-o Port=` wins over an earlier `-p` (last write wins in the -o pass).
        assert_eq!(p.profile.port, 2200);
    }

    #[test]
    fn explicit_flags_override_target_userhost() {
        // `-l` / `-p` override the `user@host:port` on the target.
        let p = parse_ssh_connect_input("ssh me@host:22 -l other -p 2200").unwrap();
        assert_eq!(p.profile.user, "other");
        assert_eq!(p.profile.port, 2200);
    }

    #[test]
    fn rejects_bad_typed_connect_lines() {
        // No host at all.
        assert!(parse_ssh_connect_input("ssh -p 2222").is_err());
        // A remote command or `--` separator is not a connect line.
        assert!(parse_ssh_connect_input("ssh dev uptime").is_err());
        assert!(parse_ssh_connect_input("ssh -- dev").is_err());
        // Unbalanced quote.
        assert!(parse_ssh_connect_input("ssh 'host").is_err());
        // Invalid port.
        assert!(parse_ssh_connect_input("ssh host -p 0").is_err());
    }
}

#[cfg(test)]
mod keybinding_gpui_tests {
    use crate::core::config::Config;
    use crate::core::session::Session;
    use crate::ui::app::Tty7App;
    use crate::ui::settings::SettingsSection;
    use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};

    fn harness(cx: &mut TestAppContext) -> (Entity<Tty7App>, VisualTestContext) {
        // The pause-to-commit is a real `smol::Timer` (off the deterministic
        // executor), so waiting on it parks the test thread.
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            crate::ui::keymap::init(cx);
        });
        // Wrap the app in a `gpui_component::Root` exactly like `main.rs` does:
        // the settings overlay's search box (and other gpui-component widgets)
        // reach for `Root` on the window, which panics if the window's first
        // layer isn't one. `Root::view()` hands the typed app entity back so the
        // tests still drive `Tty7App` directly.
        let window = cx.add_window(|window, cx| {
            let app = cx.new(|cx| Tty7App::with_session(Some(Session::default()), window, cx));
            gpui_component::Root::new(app, window, cx)
        });
        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.background_executor.run_until_parked();
        let app = window
            .update(cx, |root, _, _| {
                root.view()
                    .clone()
                    .downcast::<Tty7App>()
                    .ok()
                    .expect("window root wraps a Tty7App")
            })
            .unwrap();
        let vcx = VisualTestContext::from_window(window.into(), cx);
        (app, vcx)
    }

    /// Open Settings → Keybindings and begin capturing `action`.
    fn begin_capture(app: &Entity<Tty7App>, vcx: &mut VisualTestContext, action: &str) {
        let action = action.to_string();
        app.update_in(vcx, |app, window, cx| {
            app.toggle_settings(window, cx);
            app.select_settings_section(SettingsSection::Keybindings, cx);
            app.start_recording_key(action, window, cx);
        });
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
        let (app, mut vcx) = harness(cx);
        begin_capture(&app, &mut vcx, "NewTab");
        // The platform-primary modifier normalizes to `secondary` on write.
        vcx.simulate_keystrokes("secondary-shift-n");
        wait_for_binding(&mut vcx, "NewTab", "secondary-shift-n");

        let recording = app.update_in(&mut vcx, |app, _, _| {
            app.active_settings().map(|s| s.recording.is_some())
        });
        assert_eq!(
            recording,
            Some(false),
            "capture should end after committing"
        );
    }

    // A two-chord sequence (the tmux-style `ctrl-b x`) records as one binding.
    #[gpui::test]
    fn recording_a_two_chord_sequence_writes_the_full_spec(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);
        begin_capture(&app, &mut vcx, "CloseActiveTab");
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
        let (app, mut vcx) = harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.toggle_settings(window, cx);
            app.select_settings_section(SettingsSection::Keybindings, cx);
            app.start_recording_key("NewTab".to_string(), window, cx);
        });
        vcx.simulate_keystrokes("escape");
        vcx.background_executor.run_until_parked();

        let stored = vcx.update(|_, cx| cx.global::<Config>().keybindings.contains_key("NewTab"));
        assert!(!stored, "Esc must not persist a binding");
        let recording = app.update_in(&mut vcx, |app, _, _| {
            app.active_settings().map(|s| s.recording.is_some())
        });
        assert_eq!(recording, Some(false));
    }
}

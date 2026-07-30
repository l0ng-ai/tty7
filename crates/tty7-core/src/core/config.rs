//! User configuration loaded from `~/.config/tty7/config.json`.
//!
//! Every field is optional in the file: a missing or malformed config falls back
//! to the built-in defaults (which mirror the values previously hardcoded across
//! the app), so the terminal always starts cleanly. Parse failures are logged via
//! `log::warn!` rather than panicking.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

/// The OpenType features configured for terminal text, as an ordered tag → value
/// list (`[("calt", 1), ("liga", 1)]`).
///
/// This is a deliberate, behavior-identical replica of `gpui::FontFeatures`: the
/// field it backs is a real key in the user's `config.json`, so its wire format
/// is frozen, but `Config` itself has to parse on a headless machine that never
/// links gpui. The GUI crate converts this into the gpui type at the one place
/// it hands features to the text system (`ui::app::gpui_font_features`), and a
/// test there pins the two serializations together.
///
/// Wire format, matching gpui byte for byte:
/// - a JSON object of four-character alphanumeric tags to `true` / `false` /
///   a non-negative integer;
/// - `true` → 1, `false` → 0, an integer passes through;
/// - a tag that isn't four alphanumeric characters, a negative or fractional
///   value, or a `null` value is logged and skipped rather than failing the
///   whole config parse;
/// - serialization always writes integers, so `{"calt":true}` round-trips as
///   `{"calt":1}`.
#[derive(Default, Clone, Eq, PartialEq, Hash)]
pub struct FontFeatures(pub Arc<Vec<(String, u32)>>);

impl FontFeatures {
    /// The tag → value pairs, in the order they were parsed.
    pub fn tag_value_list(&self) -> &[(String, u32)] {
        self.0.as_slice()
    }

    /// Whether `calt` is enabled, or `None` when the feature isn't present.
    pub fn is_calt_enabled(&self) -> Option<bool> {
        self.0
            .iter()
            .find(|(feature, _)| feature == "calt")
            .map(|(_, value)| *value == 1)
    }
}

impl std::fmt::Debug for FontFeatures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("FontFeatures");
        for (tag, value) in self.tag_value_list() {
            debug.field(tag, value);
        }
        debug.finish()
    }
}

/// A feature value as it appears in `config.json`: `true`/`false` or a number.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum FeatureValue {
    Bool(bool),
    Number(serde_json::Number),
}

/// A tag in the OpenType sense: exactly four ASCII alphanumerics (`calt`,
/// `ss01`, `zero`).
fn is_valid_feature_tag(tag: &str) -> bool {
    tag.len() == 4 && tag.chars().all(|c| c.is_ascii_alphanumeric())
}

impl<'de> serde::Deserialize<'de> for FontFeatures {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};

        struct FontFeaturesVisitor;

        impl<'de> Visitor<'de> for FontFeaturesVisitor {
            type Value = FontFeatures;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a map of font features")
            }

            fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut feature_list = Vec::new();
                while let Some((key, value)) =
                    access.next_entry::<String, Option<FeatureValue>>()?
                {
                    if !is_valid_feature_tag(&key) {
                        log::error!("Incorrect font feature tag: {key}");
                        continue;
                    }
                    let Some(value) = value else { continue };
                    match value {
                        FeatureValue::Bool(enable) => {
                            feature_list.push((key, u32::from(enable)));
                        }
                        FeatureValue::Number(value) => match value.as_u64() {
                            Some(value) => feature_list.push((key, value as u32)),
                            None => {
                                log::error!(
                                    "Incorrect font feature value {value} for feature tag {key}"
                                );
                                continue;
                            }
                        },
                    }
                }
                Ok(FontFeatures(Arc::new(feature_list)))
            }
        }

        deserializer.deserialize_map(FontFeaturesVisitor)
    }
}

impl serde::Serialize for FontFeatures {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        for (tag, value) in self.tag_value_list() {
            map.serialize_entry(tag, value)?;
        }
        map.end()
    }
}

/// Top-level configuration. The GUI installs it as a GPUI global (through
/// `ui`'s `ConfigGlobal` wrapper) so any view can read it; the headless server
/// just holds it.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Primary monospace font face.
    pub font_family: String,
    /// Fallback faces tried, in order, for glyphs the primary lacks.
    pub font_fallbacks: Vec<String>,
    /// Optional distinct face for bold cells. `None` reuses `font_family` with a
    /// synthesized bold weight (the current behavior).
    pub font_family_bold: Option<String>,
    /// Optional distinct face for italic cells. `None` reuses `font_family` with a
    /// synthesized italic slant.
    pub font_family_italic: Option<String>,
    /// Optional OpenType font features for terminal text. When absent, tty7 keeps
    /// terminal-safe defaults and disables contextual ligatures; when present,
    /// this map is handed to gpui's text system as-is (for example
    /// `{ "calt": true }`) — see [`FontFeatures`].
    pub font_features: Option<FontFeatures>,
    /// Base font size in pixels.
    pub font_size: f32,
    /// Line height as a multiple of the font size (e.g. 1.35 → a 13px font gets
    /// ~18px rows). Larger values loosen the vertical rhythm; smaller ones pack
    /// rows tighter. Clamped to a sane range when applied.
    pub line_height: f32,
    /// Startup theme mode: "dark" or "light".
    pub theme: String,
    /// Selected color theme id. Resolves against the theme registry (built-ins +
    /// `~/.config/tty7/themes/*`); unknown ids fall back to the default theme. The
    /// native chrome is forced to match the theme's light/dark brightness.
    pub theme_preset: String,
    /// Follow the OS light/dark appearance. When `true` the active theme is
    /// resolved from `theme_preset_light` / `theme_preset_dark` by the current
    /// system appearance (switching live when the OS mode flips) and
    /// `theme_preset` is ignored; the native chrome follows the OS instead of
    /// being pinned to the theme.
    pub theme_follow_system: bool,
    /// Theme id used while `theme_follow_system` is on and the OS is in light
    /// mode. Same registry/fallback rules as `theme_preset`.
    pub theme_preset_light: String,
    /// Theme id used while `theme_follow_system` is on and the OS is in dark
    /// mode. Same registry/fallback rules as `theme_preset`.
    pub theme_preset_dark: String,
    /// Global window-opacity override, 0.2–1.0. `None` (the default) follows the
    /// active theme's own `opacity`; when set it applies to every theme, so a
    /// chosen translucency survives theme switches.
    pub window_opacity: Option<f32>,
    /// Global window-blur override. `None` follows the active theme's `blur`.
    pub window_blur: Option<bool>,
    /// Fade unfocused panes in a split tab so the focused terminal reads as
    /// foreground. On by default; when off every pane renders at full opacity
    /// and only focus (cursor, etc.) distinguishes the active one.
    #[serde(default = "default_true")]
    pub dim_inactive_panes: bool,
    /// Optional keybinding overrides: action name (e.g. "NewTab") → keystroke
    /// (e.g. "secondary-t", which is ⌘ on macOS and Ctrl elsewhere). Unknown
    /// actions and unparseable keystrokes are ignored (with a warning) so a bad
    /// entry never blocks startup.
    pub keybindings: HashMap<String, String>,
    /// Keybinding preset layered between the built-in defaults and the user's
    /// `keybindings` overrides. `"default"` (the default) adds nothing; `"tmux"`
    /// remaps pane/tab actions onto `prefix`-led sequences (e.g. `ctrl-b c`).
    /// Parsed leniently — an unknown value resolves back to the default preset.
    #[serde(default = "default_preset")]
    pub keybinding_preset: String,
    /// The prefix chord the `tmux` preset builds its sequences from (tmux's
    /// `C-b`). Only meaningful when `keybinding_preset` is `"tmux"`. Validated as
    /// a gpui keystroke where it's consumed; a common alternative is `ctrl-a`.
    #[serde(default = "default_prefix")]
    pub prefix: String,
    /// Optional shell override for the terminals tty7 spawns. When unset (the
    /// default), the platform's default shell is used: the user's login shell on
    /// Unix (via `$SHELL`), and PowerShell on Windows (PowerShell 7 when
    /// installed, else Windows PowerShell). Set this to run a specific shell
    /// instead — e.g. `cmd` / WSL `bash` on Windows, or `fish` / `bash` on Unix.
    pub shell: Option<ShellConfig>,

    // ── Behavior ────────────────────────────────────────────────────────────
    /// Detect URLs (OSC 8 hyperlinks + bare URLs in the text), underline them on
    /// hover, and open them on ⌘/Ctrl-click. On by default.
    pub link_url: bool,
    /// Optional command template run when ⌘/Ctrl-clicking a detected file-path
    /// link, instead of tty7's built-in "open in the default app" behavior. The
    /// template is tokenized on whitespace and the placeholders `{path}`,
    /// `{line}`, and `{column}` are substituted per argument; an argument that
    /// contains a placeholder with no value (e.g. `{line}` on a link that has no
    /// line number) is dropped. `None` (the default) keeps the built-in open.
    /// Example: `"herdr edit {path} --line {line}"`.
    pub link_file_command: Option<String>,
    /// When a pane is in a detected SSH session, Command-clicking loopback URLs
    /// opens them through a temporary local SSH port-forward. Off by default
    /// because it starts background `ssh` processes.
    pub ssh_loopback_forward: bool,
    /// Blink the block cursor while the terminal is focused. On by default; when
    /// off the cursor stays solid.
    pub cursor_blink: bool,
    /// Scrollback lines kept per pane. Clamped to alacritty's ceiling (100 000)
    /// in `sanitize`. Only applies to newly spawned/attached panes.
    pub scrollback_limit: usize,
    /// Where a newly opened tab lands relative to the active one.
    #[serde(default, deserialize_with = "de_lenient")]
    pub new_tab_position: NewTabPosition,
    /// Where the tab bar is rendered: a vertical list down the left side
    /// (`left`, the default) or a horizontal strip in the title bar (`top`).
    #[serde(default, deserialize_with = "de_lenient")]
    pub tab_bar_position: TabBarPosition,
    /// Width (px) of the vertical tab sidebar (only meaningful when
    /// `tab_bar_position` is `left`). Set by dragging the sidebar's right edge;
    /// the live layout re-clamps it to `[180, window_width/2]`.
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    /// Whether the vertical tab sidebar starts collapsed out of the layout (only
    /// meaningful when `tab_bar_position` is `left`). Distinct from
    /// `tab_bar_position`: collapsing hides the rail *without* falling back to
    /// the horizontal title-bar strip, so the terminal gets the full width and
    /// re-expanding restores the same rail. Toggled by `ToggleLeftPanel`.
    ///
    /// Like `right_panel_visible` and `right_panel_tab` below, this is the value
    /// a *newly opened window* starts with, not the live state of any window on
    /// screen — that lives on [`Tty7App`](crate::ui::app::Tty7App), so toggling
    /// one window's chrome leaves every other window alone. Each toggle writes
    /// back here, so a new window inherits the last choice made anywhere.
    #[serde(default)]
    pub sidebar_collapsed: bool,
    /// Whether the right detail panel (session info / changes / files) starts
    /// docked open. Toggled by `ToggleRightPanel`. Per-window at runtime — see
    /// `sidebar_collapsed`.
    #[serde(default)]
    pub right_panel_visible: bool,
    /// Width (px) of the right detail panel. Re-clamped by the live layout the
    /// same way `sidebar_width` is. Unlike the two flags around it this stays
    /// shared: a width is a preference, not a view state, and every window
    /// tracking the config is what makes a drag in one hold in the next.
    #[serde(default = "default_right_panel_width")]
    pub right_panel_width: f32,
    /// Which tab the right detail panel starts on, so reopening it lands where
    /// it was left. Per-window at runtime — see `sidebar_collapsed`.
    #[serde(default, deserialize_with = "de_lenient")]
    pub right_panel_tab: RightPanelTab,
    /// How the vertical tab sidebar arranges its rows (only meaningful when
    /// `tab_bar_position` is `left`): grouped under a header per git work tree
    /// (`repo`, the default), or one flat list (`none`).
    #[serde(default, deserialize_with = "de_lenient")]
    pub sidebar_grouping: SidebarGrouping,
    /// Whether clicking a sidebar row's `+N −N` working-tree counts opens the
    /// diff overlay. Off leaves the branch and the counts exactly as they are —
    /// they are a readout worth having on their own — and only takes away the
    /// click target and its pointer cursor, so the press falls through to
    /// ordinary tab activation. On by default: the overlay is the reason the
    /// counts are there for most people, and the large-diff cost it used to
    /// carry is now bounded by the diff parser's budgets. This is the escape
    /// hatch for anyone who wants the numbers without the viewer.
    #[serde(default = "default_true")]
    pub sidebar_diff_preview: bool,
    /// When to post a desktop notification after a long foreground command
    /// finishes.
    #[serde(default, deserialize_with = "de_lenient")]
    pub notify_on_command_finish: NotifyMode,
    /// On startup, ask GitHub whether a newer release has shipped and, if so,
    /// surface a "download" prompt in Settings → About. Never downloads or
    /// self-updates — it only links to the Releases page. On by default; set to
    /// `false` to skip the network call entirely (offline / privacy).
    pub check_for_updates: bool,
    /// Seconds a foreground command must run before it's eligible for a
    /// "command finished" notification (further gated by
    /// `notify_on_command_finish`). Defaults to 10; clamped in `sanitize` so a
    /// hand-edit can't set a degenerate value.
    #[serde(default = "default_notify_threshold_secs")]
    pub notify_threshold_secs: u64,
    /// Restore the previous session (tab/split layout + each pane's cwd) on
    /// launch. On by default; when off, every launch starts with a single fresh
    /// terminal instead of the last window's layout. The session is still saved
    /// on quit — it's just ignored at startup.
    #[serde(default = "default_true")]
    pub restore_session: bool,
    /// Show the system tray / menu bar status item: the icon flips to an
    /// attention state when a coding agent needs input, and its menu lists the
    /// agent panes. On by default; the tray's poll loop re-reads this every
    /// second, so toggling it (Settings or a `config.json` edit) applies live.
    #[serde(default = "default_true")]
    pub show_tray_icon: bool,
    /// Ask before closing the *last* window (the close that also quits the app).
    /// On by default, which is the behavior every build so far has had.
    ///
    /// The prompt was only ever a teaching device, not a safety net: ⌘Q, the
    /// tray's Quit and the palette's Quit all leave without asking, and nothing
    /// is lost either way — the panes keep running in the daemon. So once the
    /// user has learned that (Settings states it permanently under "How sessions
    /// work"), being asked on every quit is pure friction. Off makes the last
    /// window close exactly like any other: detach the workspace, quit.
    #[serde(default = "default_true")]
    pub confirm_window_close: bool,
    /// How the terminal bell (BEL / `^G`) is signalled. Defaults to a brief
    /// visual flash (the current behavior).
    #[serde(default, deserialize_with = "de_lenient")]
    pub bell: BellMode,
    /// Tab at the prompt opens tty7's own completion menu (commands, paths,
    /// per-command signatures). On by default. When off — or whenever the
    /// engine has nothing to offer — the prompt line is handed to the shell
    /// and Tab goes to the PTY, so the shell's native completion (compsys,
    /// fzf-tab, …) answers instead.
    #[serde(default = "default_true")]
    pub tab_completion: bool,
    /// Ctrl+R at the prompt opens tty7's fuzzy history menu. On by default.
    /// When off, the prompt line is handed to the shell and Ctrl+R goes to the
    /// PTY, so whatever is bound there answers instead — readline/zle's own
    /// reverse-i-search, or a widget like fzf's or percol's (#163).
    #[serde(default = "default_true")]
    pub history_search: bool,

    // ── Appearance ──────────────────────────────────────────────────────────
    /// The shape drawn for the terminal cursor.
    #[serde(default, deserialize_with = "de_lenient")]
    pub cursor_style: CursorStyle,

    // ── Input / Mouse ───────────────────────────────────────────────────────
    /// macOS only: treat the Option (⌥) key as Alt/Meta. On, an Option chord
    /// sends the ESC-prefixed sequence Meta bindings expect (Option+B → `ESC b`,
    /// readline's backward-word), like Ghostty's `macos-option-as-alt` /
    /// iTerm2's "Option as Meta". Off (the default), Option keeps its macOS
    /// role of composing special characters (Option+B → `∫`). Ignored on other
    /// platforms, where Alt always carries the Meta meaning.
    pub macos_option_as_alt: bool,
    /// Hide the OS mouse pointer while typing; it reappears on the next mouse
    /// move. Off by default.
    pub mouse_hide_while_typing: bool,
    /// Focus a pane as soon as the mouse moves over it, without a click. Off by
    /// default; handy with split panes.
    pub focus_follows_mouse: bool,
    /// Multiplier applied to mouse-wheel scroll distance. 1.0 = one row per wheel
    /// line (the raw amount). Clamped to a sane band in `sanitize`.
    pub mouse_scroll_multiplier: f32,
    /// Report mouse events (click / drag / wheel) to full-screen apps that ask
    /// for them (vim, tmux, htop). On by default. When off, the mouse always
    /// stays local — native selection and scrollback — regardless of what the
    /// app requested. Holding Shift already forces local behavior for a single
    /// gesture even while this is on.
    #[serde(default = "default_true")]
    pub mouse_reporting: bool,
    /// Drop trailing whitespace from each copied line. Off by default.
    pub clipboard_trim_trailing_spaces: bool,
    /// Copy a mouse selection to the clipboard as soon as the gesture ends,
    /// without ⌘C (à la Ghostty/iTerm2's copy-on-select). Off by default —
    /// the clipboard is never overwritten by a stray selection unless opted
    /// into.
    pub copy_on_select: bool,
    /// Double-click smart selection: expand the selection to the whole URL,
    /// email address, file path, or matching bracket pair under the cursor
    /// when the plain word sits inside one. On by default; off restores the
    /// bare word-boundary double-click.
    #[serde(default = "default_true")]
    pub smart_select: bool,
    /// Characters (besides whitespace) that end a double-click word
    /// selection, in both the terminal grid and the prompt's command editor.
    /// The default mirrors alacritty's semantic escape set — note `/ . - _`
    /// are *not* separators, so paths select as one word. JSON-only (no GUI
    /// widget yet).
    #[serde(default = "default_word_separators")]
    pub word_separators: String,
    /// Window state at launch: normal / maximized / fullscreen.
    #[serde(default, deserialize_with = "de_lenient")]
    pub startup_mode: StartupMode,
    /// Reopen a normal (non-maximized/fullscreen) startup window at the size
    /// and position it had when tty7 last quit. On by default; off opens
    /// centered at the built-in default size. The remembered geometry itself
    /// lives in `window.json` (see [`crate::core::window_state`]), not here.
    #[serde(default = "default_true")]
    pub remember_window_size: bool,

    // ── Shell environment ───────────────────────────────────────────────────
    /// Where a shell starts when the client doesn't pass an explicit directory
    /// (a new tab inheriting the active pane's cwd, or session restore, always
    /// win over this).
    #[serde(default)]
    pub working_directory: WorkingDirectory,
    /// Extra environment variables injected into every spawned shell, on top of
    /// the inherited environment. Currently JSON-only (no GUI widget yet); a
    /// key/value editor is a future addition.
    #[serde(default)]
    pub env: HashMap<String, String>,

    // ── SSH connection manager ───────────────────────────────────────────────
    /// Saved SSH connection profiles (the connection-manager data layer). Secrets
    /// never live here — a profile only carries a `credential_ref` naming its OS
    /// keychain entry (see [`crate::core::keychain`]). This is distinct from the
    /// live `ssh_config` alias *discovery* in [`crate::core::ssh_config`]: these
    /// are user-owned, editable profiles that can be imported from `~/.ssh/config`.
    #[serde(default)]
    pub ssh_profiles: Vec<crate::core::ssh_profile::SshProfile>,
    /// Global default for verifying SSH host keys against `known_hosts` on the
    /// native (russh) path. On by default (never weaken security silently). A
    /// per-profile `verify_host_keys` override wins over this when set; this is
    /// the fallback when a profile leaves it unset and for QuickConnect. Turning
    /// it off disables unknown/changed-host-key confirmation entirely — a
    /// deliberate, documented escape hatch (PRD FR-S4).
    #[serde(default = "default_true")]
    pub verify_host_keys: bool,
    /// Global default for the "confirm before closing a live SSH session"
    /// prompt (PRD FR-E3). Off by default (closing is unsurprising for most
    /// panes). A per-profile `warn_on_close: Some(true/false)` override wins over
    /// this when set; this is the fallback for profiles that leave it unset and
    /// for QuickConnect panes.
    #[serde(default)]
    pub ssh_warn_on_close: bool,
    /// Per-profile usage stats driving the palette's frecency ordering (PRD
    /// FR-P3): a saved profile's id → how many times it was connected and when it
    /// was last used. Bumped on every connect; read to rank the palette's profile
    /// rows. Entries for deleted profiles are harmless (never surfaced).
    #[serde(default)]
    pub ssh_profile_frecency: HashMap<uuid::Uuid, ProfileUsage>,

    /// Per-command usage for the palette's "Recent" group, keyed by the stable
    /// id in `ui::palette::CommandKind::id`. The static command list is ordered
    /// by hand, which means the first screenful is whatever the author typed
    /// first rather than what this user actually runs; this is what lets the
    /// palette lead with the latter. Only commands with a stable id are tracked
    /// — a "switch to tab 3" is not a thing to be recently-used.
    #[serde(default)]
    pub command_frecency: HashMap<String, ProfileUsage>,

    // ── CLI coding agents ────────────────────────────────────────────────────
    /// User-defined agent-detection rules: a command basename → an agent slug
    /// (`{"cc": "claude", "my-codex": "codex"}`), so personal wrappers get
    /// branded like the agent they launch. Complements the built-in registry in
    /// [`crate::core::cli_agent`]; built-ins win on their own names. The daemon
    /// reads this once per process (restart the daemon to apply changes).
    #[serde(default)]
    pub agent_commands: HashMap<String, String>,
    /// On session restore, when a pane can't re-attach (the daemon lost it —
    /// reboot, daemon restart) but it was running a coding agent whose native
    /// session id we captured, type that agent's resume command into the fresh
    /// shell (`claude --resume <id>`, `codex resume <id>`, …) so the
    /// conversation continues where it left off. cmux-style; on by default.
    #[serde(default = "default_true")]
    pub restore_agent_sessions: bool,
}

/// One saved profile's usage record for palette frecency (see
/// [`Config::ssh_profile_frecency`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ProfileUsage {
    /// Times this profile has been connected.
    pub count: u32,
    /// Unix timestamp (seconds) of the most recent connect.
    pub last_used: u64,
}

impl ProfileUsage {
    /// A frecency score combining frequency (how often) with recency (how
    /// recently), so the palette floats both heavily-used and just-used profiles
    /// to the top. Recency decays smoothly over days; `now` is unix seconds.
    pub fn score(&self, now: u64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let age_days = now.saturating_sub(self.last_used) as f64 / 86_400.0;
        // Frequency, discounted by how stale the last use is (halves ~weekly).
        self.count as f64 / (1.0 + age_days / 7.0)
    }
}

/// The current unix time in whole seconds (0 before the epoch, which never
/// happens). Used to stamp [`ProfileUsage::last_used`].
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Policy for a shell's starting directory (see [`Config::working_directory`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct WorkingDirectory {
    /// Which base directory to use.
    #[serde(deserialize_with = "de_lenient")]
    pub strategy: WdStrategy,
    /// The directory used when `strategy` is [`WdStrategy::Custom`]. Kept even
    /// while another strategy is active so toggling back restores the last path.
    pub path: String,
}

/// The base-directory strategy for a freshly spawned shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WdStrategy {
    /// Inherit the daemon's current directory (falling back to `$HOME` when it's
    /// unavailable / a bare `/`). The current behavior.
    #[default]
    Inherit,
    /// Always start in the user's home directory.
    Home,
    /// Always start in [`WorkingDirectory::path`].
    Custom,
}

/// Window state applied when tty7 launches (see [`Config::startup_mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartupMode {
    /// A regular centered window at the default size (the current behavior).
    #[default]
    Normal,
    /// Maximized (zoomed) to fill the work area.
    Maximized,
    /// Native fullscreen.
    Fullscreen,
}

/// The shape drawn for the block cursor (see [`Config::cursor_style`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CursorStyle {
    /// A filled rectangle covering the whole cell (the classic block).
    #[default]
    Block,
    /// A thin vertical bar at the cell's left edge (i-beam).
    Bar,
    /// A thin horizontal line along the cell's baseline.
    Underline,
}

/// Where [`Config::new_tab_position`] inserts a freshly opened tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NewTabPosition {
    /// Immediately after the currently active tab (the current behavior).
    #[default]
    AfterCurrent,
    /// At the very end of the tab strip.
    End,
}

/// Where the tab bar is rendered (see [`Config::tab_bar_position`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TabBarPosition {
    /// A horizontal strip of chips in the title bar.
    Top,
    /// A vertical list down the left side of the window (a tab sidebar).
    #[default]
    Left,
}

/// How the vertical tab sidebar arranges its rows (see
/// [`Config::sidebar_grouping`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SidebarGrouping {
    /// Group tabs under a header per git repository (linked worktrees fold
    /// into their main checkout's group), with non-repo tabs
    /// collected in a trailing "Scratch" group. Branch changes and cds inside
    /// a repo never move a tab; only changing repos does.
    #[default]
    Repo,
    /// One flat list in tab order (the pre-grouping behavior).
    None,
}

/// When tty7 posts a "command finished" desktop notification (see
/// [`Config::notify_on_command_finish`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotifyMode {
    /// Never notify.
    Never,
    /// Only when the window is not currently focused (the current behavior).
    #[default]
    Unfocused,
    /// Always, even when the window is focused.
    Always,
}

/// How the terminal bell (BEL / `^G`) is signalled (see [`Config::bell`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BellMode {
    /// Ignore the bell entirely — no flash, no sound.
    None,
    /// A brief visual flash of the terminal (the current behavior).
    #[default]
    Visual,
    /// Ring the system bell. On platforms without one, falls back to a flash so
    /// an opted-in bell is never silent.
    Audible,
}

/// A shell program plus its launch arguments. Mirrors `alacritty_terminal`'s
/// `tty::Shell`, but lives here so config has no dependency on the PTY crate and
/// the daemon can read it straight from `config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ShellConfig {
    /// Executable to launch. Either a bare name resolved via `PATH`
    /// (e.g. `"pwsh"`, `"bash"`) or an absolute path
    /// (e.g. `"C:\\Windows\\System32\\cmd.exe"`, `"/usr/bin/fish"`).
    pub program: String,
    /// Arguments passed to the shell on launch (e.g. `["-l"]` for a login shell,
    /// or `["-NoLogo"]` for PowerShell). Empty by default.
    #[serde(default)]
    pub args: Vec<String>,
}

/// The default fallback chain for the platform we were built for.
///
/// Fallbacks are resolved by *family name* against installed fonts, so a list
/// that reads well on one OS can miss entirely on another: every name in the
/// original list (Menlo, Apple Color Emoji) shipped with macOS, which left
/// Windows and Linux with a chain that matched nothing and fell straight
/// through to the platform's own cascade.
///
/// That fall-through is not merely cosmetic. `element.rs` pins each wide cell
/// to `2 × cell_width`, and with the bundled Hack primary a cell is 0.60205em,
/// so a two-column slot is 1.2041em. The OS cascade serves a *1.0em* CJK face
/// (Microsoft YaHei, PingFang, Noto Sans CJK), and `force_width` left-aligns —
/// the ideograph hugs the left of its slot and dumps the whole 0.2em remainder
/// on the right. Naming a 1.2em face first (Maple Mono NF CN: 0.6em Latin,
/// 1.2em CJK — an exact two-cell fit) keeps the ink centered instead.
///
/// Maple is referenced by name only, never bundled — it is ~20MB per weight.
/// Users who lack it land on the stock CJK face below, which still renders;
/// it just carries the left-hugging tracking described above.
pub fn default_font_fallbacks() -> Vec<String> {
    let names: &[&str] = if cfg!(target_os = "macos") {
        &[
            "Menlo",
            "Hasklug Nerd Font Mono",
            "Maple Mono NF CN",
            "PingFang SC",
            "Apple Color Emoji",
        ]
    } else if cfg!(target_os = "windows") {
        &[
            "Maple Mono NF CN",
            "Cascadia Mono",
            "Microsoft YaHei",
            "Segoe UI Emoji",
        ]
    } else {
        &[
            "Maple Mono NF CN",
            "DejaVu Sans Mono",
            "Noto Sans CJK SC",
            "Noto Color Emoji",
        ]
    };
    names.iter().map(|n| n.to_string()).collect()
}

/// Stock faces this platform is expected to ship, appended to whatever the user
/// configured (see `terminal::view::fallback_chain`).
///
/// [`default_font_fallbacks`] only helps a *fresh* config. Anyone who already
/// has a `config.json` carries the old macOS-only list forever, so the same
/// repair has to happen at use time. Appending is safe by construction: a
/// fallback is consulted only once everything ahead of it has missed, so these
/// can never displace a face the user chose.
pub fn platform_last_resort_fallbacks() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["PingFang SC", "Apple Color Emoji"]
    } else if cfg!(target_os = "windows") {
        &["Microsoft YaHei", "Segoe UI Emoji"]
    } else {
        &["Noto Sans CJK SC", "Noto Color Emoji"]
    }
}

impl Default for Config {
    fn default() -> Self {
        // These defaults match the values that used to be hardcoded in
        // `TerminalView::new` and `app::apply_theme`.
        Self {
            // "Hack" is bundled with the app (see `register_bundled_fonts` in
            // main.rs), so this default renders identically everywhere without
            // relying on a system install. Menlo stays as a safety net.
            font_family: "Hack".to_string(),
            font_fallbacks: default_font_fallbacks(),
            font_family_bold: None,
            font_family_italic: None,
            font_features: None,
            font_size: 15.0,
            line_height: 1.4,
            theme: "light".to_string(),
            // The default theme id (mirrors `ui::presets::DEFAULT_ID`; core can't
            // depend on ui). Unknown ids fall back to it anyway.
            theme_preset: "light".to_string(),
            theme_follow_system: false,
            // The built-in light/dark pair; each side is user-swappable in
            // Settings once "sync with system" is on.
            theme_preset_light: "light".to_string(),
            theme_preset_dark: "dark".to_string(),
            window_opacity: None,
            window_blur: None,
            dim_inactive_panes: true,
            keybindings: HashMap::new(),
            keybinding_preset: default_preset(),
            prefix: default_prefix(),
            // `None` → the platform default shell (login shell on Unix,
            // PowerShell 7 / Windows PowerShell on Windows), chosen by the
            // daemon at spawn time.
            shell: None,
            // Behavior defaults mirror the values previously hardcoded across the
            // app, so exposing them as config changes nothing until the user opts
            // out: URL detection on, cursor blinking, 10k scrollback, new tabs
            // after the active one, notify only while unfocused.
            link_url: true,
            link_file_command: None,
            ssh_loopback_forward: false,
            cursor_blink: true,
            scrollback_limit: 10_000,
            new_tab_position: NewTabPosition::AfterCurrent,
            // Vertical sidebar down the left side; `top` opts back into the
            // horizontal title-bar strip.
            tab_bar_position: TabBarPosition::Left,
            sidebar_width: default_sidebar_width(),
            sidebar_collapsed: false,
            right_panel_visible: false,
            right_panel_width: default_right_panel_width(),
            right_panel_tab: RightPanelTab::Info,
            sidebar_grouping: SidebarGrouping::Repo,
            // Today's behaviour, unchanged: the counts open the overlay.
            sidebar_diff_preview: true,
            notify_on_command_finish: NotifyMode::Unfocused,
            // Opt-out, not opt-in: a stale terminal that never tells you it's
            // outdated is the status quo we're fixing. One cheap GET at startup.
            check_for_updates: true,
            notify_threshold_secs: default_notify_threshold_secs(),
            restore_session: true,
            show_tray_icon: true,
            confirm_window_close: true,
            // Visual flash preserves the pre-config behavior (the bell always
            // flashed); opting into None/Audible is a deliberate change.
            bell: BellMode::Visual,
            tab_completion: true,
            history_search: true,
            cursor_style: CursorStyle::Block,
            // Input/mouse defaults preserve today's behavior: Option composes
            // characters as macOS ships it (opt into Option-as-Meta); GPUI
            // already hides the pointer while typing (its `CursorHideMode`
            // default), so that starts `true`; no focus-follows-mouse, raw 1×
            // scroll, no copy trim, a normal centered window.
            macos_option_as_alt: false,
            mouse_hide_while_typing: true,
            focus_follows_mouse: false,
            mouse_scroll_multiplier: 1.0,
            mouse_reporting: true,
            clipboard_trim_trailing_spaces: false,
            copy_on_select: false,
            smart_select: true,
            word_separators: default_word_separators(),
            startup_mode: StartupMode::Normal,
            remember_window_size: true,
            working_directory: WorkingDirectory::default(),
            env: HashMap::new(),
            ssh_profiles: Vec::new(),
            verify_host_keys: true,
            ssh_warn_on_close: false,
            ssh_profile_frecency: HashMap::new(),
            command_frecency: HashMap::new(),
            agent_commands: HashMap::new(),
            restore_agent_sessions: true,
        }
    }
}

impl Config {
    /// Load the config, falling back to defaults if the file is absent or
    /// unreadable, and to defaults (with a warning) if it fails to parse.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Config::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            // Missing/unreadable config is the common case — start with defaults.
            return Config::default();
        };
        match serde_json::from_str::<Config>(strip_bom(&text)) {
            Ok(mut cfg) => {
                cfg.sanitize();
                cfg
            }
            Err(e) => {
                log::warn!(
                    "failed to parse config at {}: {e}; using defaults",
                    path.display()
                );
                Config::default()
            }
        }
    }

    /// Clamp parsed values into sane ranges so a hand-edited or corrupt
    /// `config.json` can't crash the renderer (e.g. `font_size: 0` or a tiny
    /// `line_height` would round the row height to 0 → divide-by-zero →
    /// `usize::MAX` rows → allocation panic on first paint).
    fn sanitize(&mut self) {
        if !self.font_size.is_finite() || self.font_size <= 0.0 {
            self.font_size = Config::default().font_size;
        }
        self.font_size = self.font_size.clamp(4.0, 256.0);
        if !self.line_height.is_finite() || self.line_height <= 0.0 {
            self.line_height = Config::default().line_height;
        }
        self.line_height = self.line_height.clamp(0.5, 4.0);
        // Keep scrollback in a sane band: a floor so it's never uselessly tiny,
        // and alacritty's own ceiling (a huge value would just balloon memory —
        // the emulator caps history there anyway).
        self.scrollback_limit = self.scrollback_limit.clamp(100, MAX_SCROLLBACK);
        if !self.mouse_scroll_multiplier.is_finite() || self.mouse_scroll_multiplier <= 0.0 {
            self.mouse_scroll_multiplier = Config::default().mouse_scroll_multiplier;
        }
        self.mouse_scroll_multiplier = self.mouse_scroll_multiplier.clamp(0.1, 10.0);
        // Keep the notify threshold in a usable band: a 1s floor so it can't fire
        // on every trivial command, and a 1-hour ceiling above which "long
        // command" stops meaning anything.
        self.notify_threshold_secs = self.notify_threshold_secs.clamp(1, 3600);
        // A NaN override would make the whole window invisible or poison the
        // alpha math; drop it. The floor keeps a hand-edited value from hiding
        // the window entirely.
        self.window_opacity = self
            .window_opacity
            .filter(|o| o.is_finite())
            .map(|o| o.clamp(0.2, 1.0));
        // A corrupt/NaN width would poison `w(px(..))`; keep it in a broad safe
        // band (the live layout enforces the real `[180, window/2]` bounds).
        if !self.sidebar_width.is_finite() || self.sidebar_width <= 0.0 {
            self.sidebar_width = default_sidebar_width();
        }
        self.sidebar_width = self.sidebar_width.clamp(100.0, 2000.0);
        if !self.right_panel_width.is_finite() || self.right_panel_width <= 0.0 {
            self.right_panel_width = default_right_panel_width();
        }
        self.right_panel_width = self.right_panel_width.clamp(100.0, 2000.0);
        // An empty or whitespace-only file-open command means "no override"; the
        // settings text field yields `""` when cleared, so fold it back to `None`
        // rather than trying to run an empty command.
        if let Some(command) = &self.link_file_command
            && command.trim().is_empty()
        {
            self.link_file_command = None;
        }
    }

    /// Write the current config back to disk, creating the parent directory if
    /// needed. Used to persist runtime changes (theme toggle, font zoom) so they
    /// survive a restart. Failures are logged, never fatal.
    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = write_atomic(&path, text.as_bytes()) {
                    log::warn!("failed to write config at {}: {e}", path.display());
                }
            }
            Err(e) => log::warn!("failed to serialize config: {e}"),
        }
    }

    /// `~/.config/tty7/config.json`.
    fn path() -> Option<PathBuf> {
        config_path("config.json")
    }
}

/// Process-wide override for the config directory. Set once at startup from the
/// `--config-dir` CLI flag (see `main`); `None` means "use the default". Lets a
/// dev build (`cargo dev`) keep its config/session/history out of the real
/// `~/.config/tty7/` so debugging never clobbers your live setup.
static CONFIG_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Pin the config directory for this process. Idempotent — only the first call
/// wins, so call it before any `config_path` use (i.e. before `Config::load`).
pub fn set_config_dir(dir: PathBuf) {
    let _ = CONFIG_DIR_OVERRIDE.set(dir);
}

/// The directory every config-dir file lives in. Resolution order:
/// 1. `--config-dir` override (via `set_config_dir`),
/// 2. `$TTY7_CONFIG_DIR` env var,
/// 3. the platform default (see [`default_config_dir`]).
fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = CONFIG_DIR_OVERRIDE.get() {
        return Some(dir.clone());
    }
    if let Some(dir) = std::env::var_os("TTY7_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    default_config_dir()
}

/// Default config directory on Unix: `$HOME/.config/tty7` (the XDG-ish location
/// tty7 has always used).
///
/// Public so a test guard can ask "is the dir we'd write to the *user's real
/// one*?" without re-deriving the platform layout — see the `#[cfg(test)]`
/// `Config::save` in the GUI crate's `core::config`.
#[cfg(not(windows))]
pub fn default_config_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".config/tty7"))
}

/// Default config directory on Windows: `%APPDATA%\tty7` (the conventional
/// per-user roaming app-data location), falling back to
/// `%USERPROFILE%\.config\tty7` to mirror the Unix layout if `APPDATA` is unset.
///
/// Public for the same reason as the Unix arm above.
#[cfg(windows)]
pub fn default_config_dir() -> Option<PathBuf> {
    if let Some(appdata) = std::env::var_os("APPDATA").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(appdata).join("tty7"));
    }
    let profile = std::env::var_os("USERPROFILE").filter(|d| !d.is_empty())?;
    Some(PathBuf::from(profile).join(".config").join("tty7"))
}

/// Resolve a file under the config directory (no `dirs` dep). Shared by every
/// config-dir file (`config.json`, `views.json`, `history`).
pub fn config_path(file: &str) -> Option<PathBuf> {
    Some(config_dir()?.join(file))
}

/// Drop a leading UTF-8 BOM so a hand-edited config still parses.
///
/// `serde_json` rejects U+FEFF before the opening brace, and every config-dir
/// file is read by a loader that treats *any* parse error as "there is no
/// config" — so a BOM doesn't surface as an error, it silently resets the
/// user's settings. Windows makes that easy to hit by accident: PowerShell's
/// `>`, `Out-File` and `Set-Content -Encoding utf8` all write one, so a quick
/// `... | Set-Content config.json` is enough to lose every setting.
///
/// `read_to_string` decodes the BOM to the single char U+FEFF, so this strips
/// the char rather than the three raw bytes.
pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{FEFF}').unwrap_or(text)
}

/// Write `bytes` to `path` atomically: write to a sibling temp file, fsync, then
/// rename over the target. A crash/power-loss mid-write then leaves either the
/// old file or the new one intact — never a truncated/half-written file that
/// fails to parse and silently reverts the user's settings to defaults. The temp
/// lives in the same directory so the rename stays on one filesystem (atomic).
/// Shared by `Config::save` and `WindowViews::save`.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_mode(path, bytes, false)
}

/// [`write_atomic`], with the target owner-only from the first instant its final
/// name exists.
///
/// The mode is set on the *temp* file, before the rename, for the same reason
/// [`bind_control_socket`](crate::host::server::bind_control_socket) tightens
/// the umask around its `bind` rather than chmod-ing afterwards: a fix-up on the
/// next line is a window in which the file is readable, and under a `umask 002`
/// — the default wherever user-private groups are configured — that window is
/// group-readable. For documents whose contents are the user's business alone:
/// `machine.json` names every workspace's directories, SSH users and hosts, and
/// agent session ids.
///
/// A no-op difference on Windows, which has no mode bits: the config directory's
/// own ACL is the boundary there, as it is for the daemon's port file.
pub fn write_atomic_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_mode(path, bytes, true)
}

fn write_atomic_mode(path: &std::path::Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    // Per-process-unique temp name so two concurrent writers don't clobber the
    // same scratch file (the final rename then resolves last-writer-wins, with no
    // torn target either way).
    let tmp = dir.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("out"),
        std::process::id()
    ));
    {
        let mut open = std::fs::OpenOptions::new();
        open.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt as _;
            open.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = private;
        let mut f = open.open(&tmp)?;
        f.write_all(bytes)?;
        f.flush()?;
        let _ = f.sync_all();
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// The resolved config directory, exposed so the daemon spawner can forward it to
/// the detached child as `--config-dir`. We hand the child the *resolved* path
/// rather than rely on inheritance, so the spawned daemon lands in the exact dir
/// the GUI is using (dev and prod each get their own daemon — that isolation is
/// intentional). `None` only when nothing resolves (no override, no env var, no
/// `$HOME`); the caller then omits the flag and lets the child fall back to its
/// own default resolution.
pub fn config_dir_path() -> Option<PathBuf> {
    config_dir()
}

/// The user's configured shell override, if any, as `(program, args)`. Loaded
/// straight from `config.json` so the **daemon** process (which has no GPUI
/// `Config` global) can honor it when spawning a PTY. `None` → the daemon picks
/// the platform default (login shell on Unix, PowerShell 7 / Windows PowerShell
/// on Windows).
pub fn shell_command() -> Option<(String, Vec<String>)> {
    Config::load().shell.map(|s| (s.program, s.args))
}

/// The forced base directory for a spawned shell, per `working_directory`.
/// `Some(dir)` overrides the daemon's inherit fallback (but not an explicit
/// client-supplied cwd); `None` means "use the inherit fallback" (the default).
/// Read straight from `config.json` so the **daemon** can honor it. `Home`/an
/// empty `Custom` path resolve via `$HOME`.
pub fn working_directory_base() -> Option<PathBuf> {
    let wd = Config::load().working_directory;
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    match wd.strategy {
        WdStrategy::Inherit => None,
        WdStrategy::Home => home(),
        WdStrategy::Custom => {
            let p = wd.path.trim();
            if p.is_empty() {
                home()
            } else {
                Some(PathBuf::from(p))
            }
        }
    }
}

/// Extra environment variables to inject into every spawned shell, read from
/// `config.json` on the daemon side (which has no GPUI `Config` global).
pub fn extra_env() -> HashMap<String, String> {
    Config::load().env
}

/// User-defined agent-detection rules (`agent_commands`), keys lowercased,
/// cached once per process. The daemon consults this from its 0.5 s foreground
/// poll on every pane, so it must not re-read `config.json` each time; the
/// trade-off is that rule edits apply on the next daemon start (the GUI's
/// "Restart daemon" command counts).
pub fn agent_commands_cached() -> &'static HashMap<String, String> {
    static CACHE: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        Config::load()
            .agent_commands
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect()
    })
}

/// Serde default for [`Config::keybinding_preset`]: the no-op `"default"` preset.
fn default_preset() -> String {
    "default".to_string()
}

/// Serde default for the several `bool` fields that default to `true` (so a
/// config predating them, or one omitting them, keeps the on-by-default
/// behavior instead of deserializing to `false`).
fn default_true() -> bool {
    true
}

/// Serde default for [`Config::word_separators`]: alacritty's stock semantic
/// escape set, the boundary characters double-click word selection used
/// before this was configurable.
fn default_word_separators() -> String {
    ",│`|:\"' ()[]{}<>\t".to_string()
}

/// Serde default for [`Config::notify_threshold_secs`]: the 10-second floor a
/// command had to cross before this was configurable.
fn default_notify_threshold_secs() -> u64 {
    10
}

/// Serde default for [`Config::prefix`]: tmux's classic `C-b`.
fn default_prefix() -> String {
    "ctrl-b".to_string()
}

/// Which tab the right detail panel shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RightPanelTab {
    /// Session facts: cwd, shell, branch, agent.
    #[default]
    Info,
    /// The pane's command history as a navigable outline (OSC 133 marks).
    Outline,
    /// The pane's working-tree diff.
    Changes,
    /// The file tree rooted at the pane's repository.
    Files,
}

/// Serde default for [`Config::right_panel_width`]: wide enough for a file path
/// plus its `+N −M` counts without the tree turning into an ellipsis parade.
fn default_right_panel_width() -> f32 {
    260.
}

/// Serde default for [`Config::sidebar_width`]: a comfortable rail width that
/// clears the tab labels without eating too much of the terminal.
fn default_sidebar_width() -> f32 {
    220.0
}

/// Upper bound on `scrollback_limit`. Matches alacritty_terminal's own history
/// ceiling — asking for more just wastes memory since the emulator caps there.
pub const MAX_SCROLLBACK: usize = 100_000;

/// Deserialize a field leniently: if it's present but unparseable (e.g. a typo'd
/// enum string), fall back to `Default` with a warning instead of failing the
/// whole `config.json` parse — one bad entry must never reset every other
/// setting to its default. Missing fields are still handled by the container's
/// `#[serde(default)]`, which never calls this.
pub(crate) fn de_lenient<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(&value).unwrap_or_else(|e| {
        log::warn!("ignoring invalid config value {value}: {e}; using default");
        T::default()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_usage_score_ranks_frequency_and_recency() {
        let now = 100_000_000u64;
        let day = 86_400u64;
        // Never-used scores zero.
        assert_eq!(ProfileUsage::default().score(now), 0.0);
        // Same recency, more uses ⇒ higher score.
        let a = ProfileUsage {
            count: 10,
            last_used: now,
        };
        let b = ProfileUsage {
            count: 2,
            last_used: now,
        };
        assert!(a.score(now) > b.score(now));
        // Same count, more recent ⇒ higher score (recency decays with age).
        let recent = ProfileUsage {
            count: 3,
            last_used: now,
        };
        let stale = ProfileUsage {
            count: 3,
            last_used: now - 30 * day,
        };
        assert!(recent.score(now) > stale.score(now));
    }

    #[test]
    fn ssh_warn_on_close_and_frecency_round_trip() {
        let mut cfg = Config::default();
        assert!(!cfg.ssh_warn_on_close);
        cfg.ssh_warn_on_close = true;
        let id = uuid::Uuid::new_v4();
        cfg.ssh_profile_frecency.insert(
            id,
            ProfileUsage {
                count: 4,
                last_used: 42,
            },
        );
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert!(back.ssh_warn_on_close);
        assert_eq!(back.ssh_profile_frecency.get(&id).unwrap().count, 4);
    }

    /// The sidebar diff preview is opt-*out*: a config written before the switch
    /// existed keeps today's clickable counts, and turning it off survives a
    /// write/read cycle of `config.json` (issue #239).
    #[test]
    fn sidebar_diff_preview_defaults_on_and_round_trips() {
        assert!(Config::default().sidebar_diff_preview);

        let old: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        assert!(
            old.sidebar_diff_preview,
            "absent key means today's behaviour"
        );

        let off: Config = serde_json::from_str(r#"{"sidebar_diff_preview": false}"#).unwrap();
        assert!(!off.sidebar_diff_preview);
        let json = serde_json::to_string(&off).unwrap();
        assert!(json.contains("\"sidebar_diff_preview\":false"), "persisted");
        let back: Config = serde_json::from_str(&json).unwrap();
        assert!(!back.sidebar_diff_preview);
    }

    /// Opt-*out*, unlike most flags here: a config written before this setting
    /// existed must keep the prompt, or an update would silently take away the
    /// one thing telling people their sessions survive a quit.
    #[test]
    fn confirm_window_close_defaults_on_and_round_trips() {
        assert!(Config::default().confirm_window_close);

        let old: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        assert!(old.confirm_window_close);

        let off: Config = serde_json::from_str(r#"{"confirm_window_close": false}"#).unwrap();
        assert!(!off.confirm_window_close);
        let json = serde_json::to_string(&off).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert!(!back.confirm_window_close);

        // ...and a key this build has never heard of — a config last written by
        // a newer tty7, or hand-edited — must be ignored rather than failing the
        // whole parse, which `Config::load` would swallow into *defaults*: the
        // opt-out would come back on with nothing said.
        let newer: Config =
            serde_json::from_str(r#"{"confirm_window_close": false, "not_a_setting": 7}"#).unwrap();
        assert!(!newer.confirm_window_close);
    }

    /// Also opt-*out*: every config written before the switch existed predates
    /// the choice, and those users have been looking at dimmed panes all along —
    /// defaulting to `false` would silently change how every split tab looks on
    /// upgrade. And once someone does turn it off, the `false` has to survive a
    /// save/load cycle, or the effect they opted out of returns on next launch.
    #[test]
    fn dim_inactive_panes_defaults_on_and_round_trips() {
        assert!(Config::default().dim_inactive_panes);

        let old: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        assert!(old.dim_inactive_panes);

        let off: Config = serde_json::from_str(r#"{"dim_inactive_panes": false}"#).unwrap();
        assert!(!off.dim_inactive_panes);
        let json = serde_json::to_string(&off).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert!(!back.dim_inactive_panes);
    }

    #[test]
    fn theme_follow_system_defaults_and_round_trips() {
        // Old configs (no follow-system keys) must land on off + the built-in
        // light/dark pair, so nothing changes until the user opts in.
        let cfg: Config = serde_json::from_str(r#"{"theme_preset":"dracula"}"#).unwrap();
        assert!(!cfg.theme_follow_system);
        assert_eq!(cfg.theme_preset_light, "light");
        assert_eq!(cfg.theme_preset_dark, "dark");
        assert_eq!(cfg.theme_preset, "dracula");

        let mut cfg = Config::default();
        cfg.theme_follow_system = true;
        cfg.theme_preset_light = "one_light".to_string();
        cfg.theme_preset_dark = "dracula".to_string();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert!(back.theme_follow_system);
        assert_eq!(back.theme_preset_light, "one_light");
        assert_eq!(back.theme_preset_dark, "dracula");
    }

    #[test]
    fn font_features_are_optional_and_parse_as_gpui_features() {
        let cfg: Config =
            serde_json::from_str(r#"{"font_features":{"calt":true,"liga":1}}"#).unwrap();
        let features = cfg.font_features.expect("font features should parse");
        assert_eq!(features.is_calt_enabled(), Some(true));
        assert!(
            features
                .tag_value_list()
                .iter()
                .any(|(tag, value)| tag == "liga" && *value == 1)
        );

        let default_cfg = Config::default();
        assert!(default_cfg.font_features.is_none());
    }

    /// The wire format is a real key in the user's `config.json`, so it is
    /// frozen: bools and integers both parse, both land as integers, and the
    /// integer form re-parses to the same value. (Matches what
    /// `gpui::FontFeatures` did when this field was typed as it.)
    #[test]
    fn font_features_round_trip_to_integer_valued_json() {
        let features: FontFeatures =
            serde_json::from_str(r#"{"calt":true,"liga":1,"ss01":0,"zero":false}"#).unwrap();
        assert_eq!(
            features.tag_value_list(),
            &[
                ("calt".to_string(), 1),
                ("liga".to_string(), 1),
                ("ss01".to_string(), 0),
                ("zero".to_string(), 0),
            ]
        );

        let json = serde_json::to_string(&features).unwrap();
        assert_eq!(json, r#"{"calt":1,"liga":1,"ss01":0,"zero":0}"#);
        let back: FontFeatures = serde_json::from_str(&json).unwrap();
        assert_eq!(back, features);
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }

    /// Junk inside the map is dropped, never fatal — one typo'd tag must not
    /// reset the rest of `config.json`.
    #[test]
    fn font_features_skip_bad_tags_and_values_instead_of_failing() {
        let features: FontFeatures = serde_json::from_str(
            r#"{"toolong":1,"ss":1,"calt":true,"liga":null,"kern":-1,"dlig":1.5}"#,
        )
        .unwrap();
        assert_eq!(features.tag_value_list(), &[("calt".to_string(), 1)]);
    }

    /// A `font_features` key nested in a whole config survives a full
    /// `Config` round-trip unchanged.
    #[test]
    fn font_features_survive_a_config_round_trip() {
        let cfg: Config =
            serde_json::from_str(r#"{"font_features":{"calt":true,"liga":1}}"#).unwrap();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.font_features, cfg.font_features);
        assert_eq!(
            back.font_features.as_ref().map(|f| f.tag_value_list()),
            Some(&[("calt".to_string(), 1), ("liga".to_string(), 1)][..])
        );
    }

    #[test]
    fn stale_override_keys_are_ignored() {
        // Leftover keys from the retired override system are ignored, not fatal.
        let cfg: Config = serde_json::from_str(
            r##"{"font_size": 20.0, "colors": {"border": "#fff"}, "ansi_colors": {"color1": "#f00"}}"##,
        )
        .expect("stale override keys must be ignored");
        assert_eq!(cfg.font_size, 20.0);
        assert_eq!(cfg.theme_preset, "light");
    }

    #[test]
    fn sanitize_clamps_degenerate_font_metrics() {
        // A zero/negative/NaN font size or line height would round the row height
        // to 0 and crash the renderer (divide-by-zero → usize::MAX rows). Clamp.
        let sanitized = |font_size: f32, line_height: f32| {
            let mut cfg = Config {
                font_size,
                line_height,
                ..Config::default()
            };
            cfg.sanitize();
            (cfg.font_size, cfg.line_height)
        };

        let (fs, lh) = sanitized(0.0, 0.0);
        assert!(fs >= 4.0, "font_size clamped above zero");
        assert!(lh >= 0.5, "line_height clamped above zero");

        let (fs, lh) = sanitized(f32::NAN, f32::INFINITY);
        assert!(fs.is_finite() && fs > 0.0);
        assert!(lh.is_finite() && lh > 0.0);

        // A sane value is left untouched.
        assert_eq!(sanitized(15.0, 1.4), (15.0, 1.4));
    }

    /// Per-test scratch directory, unique per test name + PID and removed on
    /// drop — cleanup runs even when an assertion panics mid-test, so a failed
    /// run can't leak state into (or collide with) the next one.
    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("tty7-test-{name}-{}", std::process::id()));
            // A stale copy from a crashed earlier run would poison this one.
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn write_atomic_replaces_contents_and_leaves_no_temp() {
        let dir = TestDir::new("atomic");
        let target = dir.path().join("data.json");
        write_atomic(&target, b"first").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");
        // Overwrite is atomic and complete (no truncation/append residue).
        write_atomic(&target, b"second-longer-and-then-short").unwrap();
        write_atomic(&target, b"3rd").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "3rd");
        // The sibling temp file must not linger.
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "temp file should be renamed away");
    }

    #[test]
    fn behavior_enums_fall_back_leniently_on_bad_values() {
        // A typo'd enum string must NOT reset the whole config: font_size is kept,
        // and only the bad field falls back to its default.
        let cfg: Config = serde_json::from_str(
            r#"{"font_size": 20.0, "new_tab_position": "middle", "notify_on_command_finish": "sometimes", "tab_bar_position": "diagonal"}"#,
        )
        .expect("a bad enum value must not fail the whole parse");
        assert_eq!(cfg.font_size, 20.0);
        assert_eq!(cfg.new_tab_position, NewTabPosition::AfterCurrent);
        assert_eq!(cfg.notify_on_command_finish, NotifyMode::Unfocused);
        assert_eq!(cfg.tab_bar_position, TabBarPosition::Left);

        // Valid kebab-case values round-trip. `top` is the non-default here, so
        // the assert still proves the field parsed rather than fell back.
        let cfg: Config = serde_json::from_str(
            r#"{"new_tab_position": "end", "notify_on_command_finish": "always", "tab_bar_position": "top"}"#,
        )
        .unwrap();
        assert_eq!(cfg.new_tab_position, NewTabPosition::End);
        assert_eq!(cfg.notify_on_command_finish, NotifyMode::Always);
        assert_eq!(cfg.tab_bar_position, TabBarPosition::Top);
    }

    #[test]
    fn working_directory_defaults_to_inherit_and_parses_kebab() {
        let cfg = Config::default();
        assert_eq!(cfg.working_directory.strategy, WdStrategy::Inherit);
        assert!(cfg.working_directory.path.is_empty());

        let cfg: Config = serde_json::from_str(
            r#"{"working_directory": {"strategy": "custom", "path": "/tmp/x"}}"#,
        )
        .unwrap();
        assert_eq!(cfg.working_directory.strategy, WdStrategy::Custom);
        assert_eq!(cfg.working_directory.path, "/tmp/x");

        // A bad strategy value falls back to the default without failing the parse.
        let cfg: Config =
            serde_json::from_str(r#"{"working_directory": {"strategy": "elsewhere"}}"#).unwrap();
        assert_eq!(cfg.working_directory.strategy, WdStrategy::Inherit);
    }

    #[test]
    fn sanitize_clamps_scroll_multiplier_into_band() {
        let clamp = |m: f32| {
            let mut cfg = Config {
                mouse_scroll_multiplier: m,
                ..Config::default()
            };
            cfg.sanitize();
            cfg.mouse_scroll_multiplier
        };
        assert_eq!(clamp(1.0), 1.0);
        assert_eq!(clamp(0.0), 1.0); // non-positive → default
        assert_eq!(clamp(-3.0), 1.0);
        assert_eq!(clamp(100.0), 10.0); // ceiling
        assert_eq!(clamp(0.01), 0.1); // floor
    }

    /// The window-opacity override is clamped into its usable band; a NaN is
    /// dropped back to "follow theme" rather than poisoning the alpha math.
    #[test]
    fn sanitize_clamps_window_opacity_override() {
        let clamp = |o: Option<f32>| {
            let mut cfg = Config {
                window_opacity: o,
                ..Config::default()
            };
            cfg.sanitize();
            cfg.window_opacity
        };
        assert_eq!(clamp(None), None);
        assert_eq!(clamp(Some(0.8)), Some(0.8));
        assert_eq!(clamp(Some(0.0)), Some(0.2)); // floor: never invisible
        assert_eq!(clamp(Some(2.0)), Some(1.0)); // ceiling
        assert_eq!(clamp(Some(f32::NAN)), None); // NaN → follow theme
    }

    #[test]
    fn sanitize_clamps_scrollback_into_band() {
        let clamp = |n: usize| {
            let mut cfg = Config {
                scrollback_limit: n,
                ..Config::default()
            };
            cfg.sanitize();
            cfg.scrollback_limit
        };
        assert_eq!(clamp(0), 100); // floor
        assert_eq!(clamp(10_000), 10_000); // untouched in-band
        assert_eq!(clamp(usize::MAX), MAX_SCROLLBACK); // ceiling
    }

    #[test]
    fn new_terminal_prefs_default_and_parse_leniently() {
        // Defaults preserve the pre-config behavior: restore on, mouse reporting
        // on, a 10s notify floor, and a visual bell.
        let cfg = Config::default();
        assert!(cfg.restore_session);
        assert!(cfg.mouse_reporting);
        assert!(cfg.tab_completion);
        assert!(cfg.history_search);
        assert_eq!(cfg.notify_threshold_secs, 10);
        assert_eq!(cfg.bell, BellMode::Visual);

        // A config predating these fields keeps the on-by-default booleans (not
        // `false`) and the 10s floor.
        let cfg: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        assert!(cfg.restore_session);
        assert!(cfg.mouse_reporting);
        assert!(cfg.tab_completion);
        assert!(cfg.history_search);
        assert_eq!(cfg.notify_threshold_secs, 10);
        assert_eq!(cfg.bell, BellMode::Visual);

        // The opt-outs round-trip.
        let cfg: Config = serde_json::from_str(r#"{"tab_completion": false}"#).unwrap();
        assert!(!cfg.tab_completion);
        let cfg: Config = serde_json::from_str(r#"{"history_search": false}"#).unwrap();
        assert!(!cfg.history_search);

        // Valid values round-trip; a bad bell string falls back without failing
        // the whole parse.
        let cfg: Config = serde_json::from_str(
            r#"{"restore_session": false, "mouse_reporting": false, "bell": "audible"}"#,
        )
        .unwrap();
        assert!(!cfg.restore_session);
        assert!(!cfg.mouse_reporting);
        assert_eq!(cfg.bell, BellMode::Audible);

        let cfg: Config = serde_json::from_str(r#"{"bell": "loud"}"#).unwrap();
        assert_eq!(cfg.bell, BellMode::Visual);
    }

    #[test]
    fn sanitize_clamps_notify_threshold_into_band() {
        let clamp = |n: u64| {
            let mut cfg = Config {
                notify_threshold_secs: n,
                ..Config::default()
            };
            cfg.sanitize();
            cfg.notify_threshold_secs
        };
        assert_eq!(clamp(0), 1); // floor
        assert_eq!(clamp(10), 10); // untouched in-band
        assert_eq!(clamp(100_000), 3600); // ceiling
    }

    #[test]
    fn keybinding_preset_and_prefix_default_and_round_trip() {
        // Missing fields fall back to the no-op preset and the tmux-classic prefix.
        let cfg = Config::default();
        assert_eq!(cfg.keybinding_preset, "default");
        assert_eq!(cfg.prefix, "ctrl-b");

        let cfg: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        assert_eq!(cfg.keybinding_preset, "default");
        assert_eq!(cfg.prefix, "ctrl-b");

        // Explicit values survive a parse.
        let cfg: Config =
            serde_json::from_str(r#"{"keybinding_preset": "tmux", "prefix": "ctrl-a"}"#).unwrap();
        assert_eq!(cfg.keybinding_preset, "tmux");
        assert_eq!(cfg.prefix, "ctrl-a");
    }

    /// A fresh config must name a CJK face the *host* platform actually ships.
    /// The pre-fix list was macOS-only, so Windows and Linux wrote a chain that
    /// matched nothing and left every ideograph to the OS cascade.
    #[test]
    fn default_font_fallbacks_are_platform_appropriate() {
        let defaults = default_font_fallbacks();
        assert!(!defaults.is_empty());

        for name in platform_last_resort_fallbacks() {
            assert!(
                defaults.iter().any(|f| f == name),
                "default chain {defaults:?} omits stock face {name}"
            );
        }

        // Maple Mono NF CN is the only face whose CJK advance (1.2em) is an exact
        // two-cell fit against the bundled Hack primary, so it must be tried
        // before the stock face on every platform.
        let maple = defaults.iter().position(|f| f == "Maple Mono NF CN");
        let maple = maple.expect("the exact-fit CJK face must stay in the chain");
        for name in platform_last_resort_fallbacks() {
            let stock = defaults.iter().position(|f| f == name).unwrap();
            assert!(maple < stock, "{name} must not preempt Maple Mono NF CN");
        }

        // macOS-only names must not leak into the other platforms' defaults.
        if !cfg!(target_os = "macos") {
            for name in ["Menlo", "Apple Color Emoji"] {
                assert!(
                    !defaults.iter().any(|f| f == name),
                    "{name} ships only with macOS"
                );
            }
        }
    }

    #[test]
    fn config_deserialize_fills_missing_fields_from_defaults() {
        // Only one field present; the rest must fall back via #[serde(default)].
        let cfg: Config = serde_json::from_str(r#"{"font_size": 20.0}"#).unwrap();
        assert_eq!(cfg.font_size, 20.0);
        assert_eq!(cfg.line_height, 1.4); // default preserved
        assert_eq!(cfg.font_family, "Hack"); // default preserved
        assert_eq!(cfg.theme_preset, "light");
        assert!(cfg.keybindings.is_empty());
    }

    /// Pin the process config dir at a shared temp location so `load`/`save` never
    /// touch the real `~/.config`. First-call-wins; every IO test uses the same path.
    fn pin_config_dir() {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        set_config_dir(dir);
    }

    /// Serialize the tests that write the shared `config.json`: they all resolve
    /// the same pinned path, so without this they clobber each other's file.
    static CONFIG_FILE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_config_file() -> std::sync::MutexGuard<'static, ()> {
        // A poisoned lock only means another test failed mid-sequence; every
        // holder rewrites the file from scratch, so the state is still sound.
        CONFIG_FILE.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn save_load_and_shell_command_round_trip_through_disk() {
        let _guard = lock_config_file();
        pin_config_dir();
        // Persist a config with a non-default shell + font + an SSH profile, then
        // read it back.
        let mut cfg = Config {
            font_size: 18.0,
            ..Config::default()
        };
        cfg.shell = Some(ShellConfig {
            program: "fish".to_string(),
            args: vec!["-l".to_string()],
        });
        let mut profile = crate::core::ssh_profile::SshProfile::new("prod-web");
        profile.host = "10.0.0.5".to_string();
        profile.user = "deploy".to_string();
        profile.port = 2222;
        profile.auth = crate::core::ssh_profile::AuthMode::PublicKey;
        profile.credential_ref = Some(crate::core::keychain::CredentialRef::password(
            "deploy", "10.0.0.5", 2222,
        ));
        cfg.ssh_profiles = vec![profile.clone()];
        cfg.save();

        let loaded = Config::load();
        assert_eq!(loaded.font_size, 18.0);
        assert_eq!(
            loaded.shell.as_ref().map(|s| s.program.as_str()),
            Some("fish")
        );
        // The SSH profile round-trips byte-for-byte, id included, with no plaintext
        // secret anywhere (only the credential *ref*).
        assert_eq!(loaded.ssh_profiles, vec![profile]);

        // `shell_command` reads the same on-disk config for the daemon side.
        let (program, args) = shell_command().expect("shell override present");
        assert_eq!(program, "fish");
        assert_eq!(args, vec!["-l".to_string()]);
    }

    #[test]
    fn a_utf8_bom_does_not_silently_reset_the_config() {
        let _guard = lock_config_file();
        pin_config_dir();
        let path = Config::path().expect("pinned config dir");
        // Exactly what PowerShell's `>`, `Out-File` and `Set-Content -Encoding
        // utf8` leave behind.
        let text = "\u{FEFF}{\"font_size\": 21.0, \"restore_session\": false}";
        write_atomic(&path, text.as_bytes()).unwrap();

        // The failure this guards is silent by construction: `load` turns *any*
        // parse error into defaults, so a BOM didn't report a bad config — it
        // reported no config, and the user's settings appeared to vanish.
        let loaded = Config::load();
        assert_eq!(loaded.font_size, 21.0);
        assert!(!loaded.restore_session);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn strip_bom_only_removes_a_leading_marker() {
        assert_eq!(strip_bom("{}"), "{}");
        assert_eq!(strip_bom("\u{FEFF}{}"), "{}");
        // Only the first U+FEFF is a marker. A second one is content, and
        // content that happens to be a BOM is still invalid JSON — stripping
        // it too would be guessing at a file we can't rescue.
        assert_eq!(strip_bom("\u{FEFF}\u{FEFF}{}"), "\u{FEFF}{}");
        // A BOM *inside* the document is data (U+FEFF is a legal string char),
        // so it must survive untouched.
        let inner = "{\"tab_title\":\"\u{FEFF}\"}";
        assert_eq!(strip_bom(inner), inner);
        assert_eq!(strip_bom(""), "");
    }

    #[test]
    fn ssh_profiles_default_empty_and_parse_from_json() {
        // Absent key → empty (a config predating profiles still loads).
        let cfg = Config::default();
        assert!(cfg.ssh_profiles.is_empty());
        let cfg: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        assert!(cfg.ssh_profiles.is_empty());

        // A present profile array parses; a bad enum value inside one profile falls
        // back leniently instead of failing the whole config parse.
        let cfg: Config = serde_json::from_str(
            r#"{"ssh_profiles":[{"name":"a","host":"h","auth":"bogus","port":2200}]}"#,
        )
        .expect("a bad per-profile enum must not fail the whole config parse");
        assert_eq!(cfg.ssh_profiles.len(), 1);
        assert_eq!(cfg.ssh_profiles[0].name, "a");
        assert_eq!(cfg.ssh_profiles[0].port, 2200);
        assert_eq!(
            cfg.ssh_profiles[0].auth,
            crate::core::ssh_profile::AuthMode::Auto
        );
    }

    #[test]
    fn config_path_resolves_under_the_pinned_dir() {
        pin_config_dir();
        let p = config_path("config.json").expect("config path resolves");
        assert!(p.ends_with("config.json"));
        // `config_dir_path` returns the same parent the files live under.
        assert_eq!(p.parent(), config_dir_path().as_deref());
    }
}

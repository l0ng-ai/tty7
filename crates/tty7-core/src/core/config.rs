use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

pub const SUPPORTED_GUI_LANGUAGES: &[&str] = &["en", "zh-CN", "ja-JP"];

#[derive(Default, Clone, Eq, PartialEq, Hash)]
pub struct FontFeatures(pub Arc<Vec<(String, u32)>>);

impl FontFeatures {
    pub fn tag_value_list(&self) -> &[(String, u32)] {
        self.0.as_slice()
    }

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

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum FeatureValue {
    Bool(bool),
    Number(serde_json::Number),
}

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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub font_family: String,
    pub font_fallbacks: Vec<String>,
    pub font_family_bold: Option<String>,
    pub font_family_italic: Option<String>,
    pub font_features: Option<FontFeatures>,
    pub font_size: f32,
    pub line_height: f32,
    pub theme: String,
    pub theme_preset: String,
    pub theme_follow_system: bool,
    pub theme_preset_light: String,
    pub theme_preset_dark: String,
    pub window_opacity: Option<f32>,
    pub window_blur: Option<bool>,
    #[serde(default = "default_true")]
    pub dim_inactive_panes: bool,
    pub keybindings: HashMap<String, String>,
    #[serde(default = "default_preset")]
    pub keybinding_preset: String,
    #[serde(default = "default_prefix")]
    pub prefix: String,
    pub shell: Option<ShellConfig>,

    pub link_url: bool,
    pub link_file_command: Option<String>,
    pub ssh_loopback_forward: bool,
    pub cursor_blink: bool,
    pub scrollback_limit: usize,
    #[serde(default, deserialize_with = "de_lenient")]
    pub new_tab_position: NewTabPosition,
    #[serde(default, deserialize_with = "de_lenient")]
    pub tab_bar_position: TabBarPosition,
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    #[serde(default)]
    pub sidebar_collapsed: bool,
    #[serde(default)]
    pub right_panel_visible: bool,
    #[serde(default = "default_right_panel_width")]
    pub right_panel_width: f32,
    #[serde(default, deserialize_with = "de_lenient")]
    pub right_panel_tab: RightPanelTab,
    #[serde(default, deserialize_with = "de_lenient")]
    pub sidebar_grouping: SidebarGrouping,
    #[serde(default = "default_true")]
    pub sidebar_diff_preview: bool,
    #[serde(default, deserialize_with = "de_lenient")]
    pub notify_on_command_finish: NotifyMode,
    pub check_for_updates: bool,
    /// Which release feed update checks follow. Stable by default, so an
    /// installation only ever ends up on Nightly by asking for it.
    #[serde(default, deserialize_with = "de_lenient")]
    pub update_channel: UpdateChannel,
    /// Whether a found update is fetched and verified before the user asks for
    /// it. On by default: it turns "spend five minutes downloading" into "press
    /// restart", which is the whole difference between an update people apply
    /// and one they postpone forever. Nothing is ever *installed* without an
    /// explicit choice — the staged package waits in Settings.
    ///
    /// Worth turning off on a metered connection: the packages run 25–30 MB and
    /// a check happens every six hours.
    #[serde(default = "default_true")]
    pub auto_download_updates: bool,
    /// Whether the GUI puts the bundled `tty7` CLI on PATH at launch (see
    /// `core::cli_install`). On by default: the CLI is the agent-facing half of
    /// this product and is worth nothing sitting unreachable inside the bundle.
    /// Off is for people who keep their own `tty7` — a `cargo install` build, a
    /// package manager's copy — and do not want it shadowed.
    #[serde(default = "default_true")]
    pub install_cli_on_path: bool,
    /// GUI-only locale selection. Values: `en` or `zh-CN`.
    /// CLI output stays English so agent/script integrations are stable.
    #[serde(default = "default_gui_language")]
    pub gui_language: String,
    #[serde(default = "default_notify_threshold_secs")]
    pub notify_threshold_secs: u64,
    #[serde(default = "default_true")]
    pub restore_session: bool,
    #[serde(default = "default_true")]
    pub show_tray_icon: bool,
    #[serde(default, deserialize_with = "de_lenient")]
    pub bell: BellMode,
    #[serde(default = "default_true")]
    pub tab_completion: bool,
    #[serde(default = "default_true")]
    pub history_search: bool,

    #[serde(default, deserialize_with = "de_lenient")]
    pub cursor_style: CursorStyle,

    pub macos_option_as_alt: bool,
    pub mouse_hide_while_typing: bool,
    pub focus_follows_mouse: bool,
    pub mouse_scroll_multiplier: f32,
    /// Spread a wheel detent over several frames instead of jumping the whole
    /// distance at once. Trackpad gestures are left alone — they are already a
    /// continuous stream, and animating one would just add lag.
    #[serde(default = "default_true")]
    pub smooth_scroll: bool,
    #[serde(default = "default_true")]
    pub mouse_reporting: bool,
    pub clipboard_trim_trailing_spaces: bool,
    pub copy_on_select: bool,
    /// Optional HTTP/SOCKS proxy for tty7's *own* update checks and release
    /// downloads; when set it overrides the system proxy and the environment.
    /// Programs running in a pane are unaffected — they inherit their proxy
    /// from their own environment, as in any other terminal.
    ///
    /// Normalised by [`Config::sanitize`], so `Some` always means a non-blank
    /// value. Examples: `http://127.0.0.1:7890`, `socks5://127.0.0.1:1080`.
    #[serde(default)]
    pub http_proxy: Option<String>,
    #[serde(default = "default_true")]
    pub smart_select: bool,
    #[serde(default = "default_word_separators")]
    pub word_separators: String,
    #[serde(default, deserialize_with = "de_lenient")]
    pub startup_mode: StartupMode,
    #[serde(default = "default_true")]
    pub remember_window_size: bool,

    #[serde(default)]
    pub working_directory: WorkingDirectory,
    #[serde(default)]
    pub env: HashMap<String, String>,

    #[serde(default)]
    pub ssh_profiles: Vec<crate::core::ssh_profile::SshProfile>,
    #[serde(default = "default_true")]
    pub verify_host_keys: bool,
    #[serde(default)]
    pub ssh_warn_on_close: bool,
    #[serde(default)]
    pub ssh_profile_frecency: HashMap<uuid::Uuid, ProfileUsage>,

    #[serde(default)]
    pub command_frecency: HashMap<String, ProfileUsage>,

    #[serde(default)]
    pub agent_commands: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub restore_agent_sessions: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct ProfileUsage {
    pub count: u32,
    pub last_used: u64,
}

impl ProfileUsage {
    pub fn score(&self, now: u64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let age_days = now.saturating_sub(self.last_used) as f64 / 86_400.0;
        self.count as f64 / (1.0 + age_days / 7.0)
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct WorkingDirectory {
    #[serde(deserialize_with = "de_lenient")]
    pub strategy: WdStrategy,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WdStrategy {
    #[default]
    Inherit,
    Home,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartupMode {
    #[default]
    Normal,
    Maximized,
    Fullscreen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CursorStyle {
    #[default]
    Block,
    Bar,
    Underline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NewTabPosition {
    #[default]
    AfterCurrent,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TabBarPosition {
    Top,
    #[default]
    Left,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SidebarGrouping {
    #[default]
    Repo,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotifyMode {
    Never,
    #[default]
    Unfocused,
    Always,
}

/// Which release feed this installation follows.
///
/// The channel is a property of the installation, not something derived from
/// the version number. Without it the only thing separating a Nightly from a
/// Stable is how their versions happen to sort, which is how a Nightly ends up
/// being walked back onto Stable by an update it never asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateChannel {
    #[default]
    Stable,
    /// Follows the rolling `nightly` prerelease, which is rebuilt from `main`
    /// every night.
    Nightly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BellMode {
    None,
    #[default]
    Visual,
    Audible,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ShellConfig {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

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
        Self {
            font_family: "Hack".to_string(),
            font_fallbacks: default_font_fallbacks(),
            font_family_bold: None,
            font_family_italic: None,
            font_features: None,
            font_size: 15.0,
            line_height: 1.4,
            theme: "light".to_string(),
            theme_preset: "light".to_string(),
            theme_follow_system: false,
            theme_preset_light: "light".to_string(),
            theme_preset_dark: "dark".to_string(),
            window_opacity: None,
            window_blur: None,
            dim_inactive_panes: true,
            keybindings: HashMap::new(),
            keybinding_preset: default_preset(),
            prefix: default_prefix(),
            shell: None,
            link_url: true,
            link_file_command: None,
            ssh_loopback_forward: false,
            cursor_blink: true,
            scrollback_limit: 10_000,
            new_tab_position: NewTabPosition::AfterCurrent,
            tab_bar_position: TabBarPosition::Left,
            sidebar_width: default_sidebar_width(),
            sidebar_collapsed: false,
            right_panel_visible: false,
            right_panel_width: default_right_panel_width(),
            right_panel_tab: RightPanelTab::Info,
            sidebar_grouping: SidebarGrouping::Repo,
            sidebar_diff_preview: true,
            notify_on_command_finish: NotifyMode::Unfocused,
            check_for_updates: true,
            update_channel: UpdateChannel::default(),
            auto_download_updates: true,
            install_cli_on_path: true,
            gui_language: default_gui_language(),
            notify_threshold_secs: default_notify_threshold_secs(),
            restore_session: true,
            show_tray_icon: true,
            bell: BellMode::Visual,
            tab_completion: true,
            history_search: true,
            cursor_style: CursorStyle::Block,
            macos_option_as_alt: false,
            mouse_hide_while_typing: true,
            focus_follows_mouse: false,
            mouse_scroll_multiplier: 1.0,
            smooth_scroll: true,
            mouse_reporting: true,
            clipboard_trim_trailing_spaces: false,
            copy_on_select: false,
            http_proxy: None,
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
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Config::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
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

    fn sanitize(&mut self) {
        if !self.font_size.is_finite() || self.font_size <= 0.0 {
            self.font_size = Config::default().font_size;
        }
        self.font_size = self.font_size.clamp(4.0, 256.0);
        if !self.line_height.is_finite() || self.line_height <= 0.0 {
            self.line_height = Config::default().line_height;
        }
        self.line_height = self.line_height.clamp(0.5, 4.0);
        self.scrollback_limit = self.scrollback_limit.clamp(100, MAX_SCROLLBACK);
        if !self.mouse_scroll_multiplier.is_finite() || self.mouse_scroll_multiplier <= 0.0 {
            self.mouse_scroll_multiplier = Config::default().mouse_scroll_multiplier;
        }
        self.mouse_scroll_multiplier = self.mouse_scroll_multiplier.clamp(0.1, 10.0);
        self.notify_threshold_secs = self.notify_threshold_secs.clamp(1, 3600);
        self.window_opacity = self
            .window_opacity
            .filter(|o| o.is_finite())
            .map(|o| o.clamp(0.2, 1.0));
        if !self.sidebar_width.is_finite() || self.sidebar_width <= 0.0 {
            self.sidebar_width = default_sidebar_width();
        }
        self.sidebar_width = self.sidebar_width.clamp(100.0, 2000.0);
        if !self.right_panel_width.is_finite() || self.right_panel_width <= 0.0 {
            self.right_panel_width = default_right_panel_width();
        }
        self.right_panel_width = self.right_panel_width.clamp(100.0, 2000.0);
        if let Some(command) = &self.link_file_command
            && command.trim().is_empty()
        {
            self.link_file_command = None;
        }
        // Normalise here so every reader can take the field as-is instead of
        // repeating a trim-and-drop-if-blank dance.
        self.http_proxy = self
            .http_proxy
            .take()
            .map(|proxy| proxy.trim().to_string())
            .filter(|proxy| !proxy.is_empty());
        if !SUPPORTED_GUI_LANGUAGES.contains(&self.gui_language.as_str()) {
            self.gui_language = default_gui_language();
        }
    }

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

    fn path() -> Option<PathBuf> {
        config_path("config.json")
    }
}

static CONFIG_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

pub fn set_config_dir(dir: PathBuf) {
    let _ = CONFIG_DIR_OVERRIDE.set(dir);
}

fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = CONFIG_DIR_OVERRIDE.get() {
        return Some(dir.clone());
    }
    if let Some(dir) = std::env::var_os("TTY7_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    default_config_dir()
}

#[cfg(not(windows))]
pub fn default_config_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".config/tty7"))
}

#[cfg(windows)]
pub fn default_config_dir() -> Option<PathBuf> {
    if let Some(appdata) = std::env::var_os("APPDATA").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(appdata).join("tty7"));
    }
    let profile = std::env::var_os("USERPROFILE").filter(|d| !d.is_empty())?;
    Some(PathBuf::from(profile).join(".config").join("tty7"))
}

pub fn config_path(file: &str) -> Option<PathBuf> {
    Some(config_dir()?.join(file))
}

pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{FEFF}').unwrap_or(text)
}

/// Sets a corrupt state file aside (copied, the original left in place) so the
/// caller can fall back to defaults without silently destroying what was there.
pub(crate) fn quarantine(path: &std::path::Path) {
    let aside = quarantine_path(path);
    match std::fs::copy(path, &aside) {
        Ok(_) => log::warn!("the previous contents were kept at {}", aside.display()),
        Err(e) => log::warn!("could not keep a copy at {}: {e}", aside.display()),
    }
}

/// Like [`quarantine`], but moves the file out of the way — for files that
/// cannot even be read, where copying would fail too.
pub(crate) fn quarantine_by_rename(path: &std::path::Path) {
    let aside = quarantine_path(path);
    match std::fs::rename(path, &aside) {
        Ok(()) => log::warn!("the previous contents were moved to {}", aside.display()),
        Err(e) => log::warn!("could not move the file to {}: {e}", aside.display()),
    }
}

fn quarantine_path(path: &std::path::Path) -> PathBuf {
    const MAX_QUARANTINED: u32 = 8;

    let base = path.with_extension("json.corrupt");
    if !base.exists() {
        return base;
    }
    (1..MAX_QUARANTINED)
        .map(|n| path.with_extension(format!("json.corrupt.{n}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or(base)
}

pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_mode(path, bytes, false)
}

pub fn write_atomic_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    write_atomic_mode(path, bytes, true)
}

fn write_atomic_mode(path: &std::path::Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
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

pub fn config_dir_path() -> Option<PathBuf> {
    config_dir()
}

pub fn shell_command() -> Option<(String, Vec<String>)> {
    Config::load().shell.map(|s| (s.program, s.args))
}

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

pub fn extra_env() -> HashMap<String, String> {
    Config::load().env
}

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

fn default_preset() -> String {
    "default".to_string()
}

fn default_true() -> bool {
    true
}

fn default_gui_language() -> String {
    "en".to_string()
}

fn default_word_separators() -> String {
    ",│`|:\"' ()[]{}<>\t".to_string()
}

fn default_notify_threshold_secs() -> u64 {
    10
}

fn default_prefix() -> String {
    "ctrl-b".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RightPanelTab {
    #[default]
    Info,
    Changes,
    Files,
}

fn default_right_panel_width() -> f32 {
    260.
}

fn default_sidebar_width() -> f32 {
    220.0
}

pub const MAX_SCROLLBACK: usize = 100_000;

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
        assert_eq!(ProfileUsage::default().score(now), 0.0);
        let a = ProfileUsage {
            count: 10,
            last_used: now,
        };
        let b = ProfileUsage {
            count: 2,
            last_used: now,
        };
        assert!(a.score(now) > b.score(now));
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

    #[test]
    fn font_features_skip_bad_tags_and_values_instead_of_failing() {
        let features: FontFeatures = serde_json::from_str(
            r#"{"toolong":1,"ss":1,"calt":true,"liga":null,"kern":-1,"dlig":1.5}"#,
        )
        .unwrap();
        assert_eq!(features.tag_value_list(), &[("calt".to_string(), 1)]);
    }

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
        let cfg: Config = serde_json::from_str(
            r##"{"font_size": 20.0, "colors": {"border": "#fff"}, "ansi_colors": {"color1": "#f00"}}"##,
        )
        .expect("stale override keys must be ignored");
        assert_eq!(cfg.font_size, 20.0);
        assert_eq!(cfg.theme_preset, "light");
    }

    #[test]
    fn sanitize_clamps_degenerate_font_metrics() {
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

        assert_eq!(sanitized(15.0, 1.4), (15.0, 1.4));
    }

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("tty7-test-{name}-{}", std::process::id()));
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
        write_atomic(&target, b"second-longer-and-then-short").unwrap();
        write_atomic(&target, b"3rd").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "3rd");
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "temp file should be renamed away");
    }

    #[test]
    fn behavior_enums_fall_back_leniently_on_bad_values() {
        let cfg: Config = serde_json::from_str(
            r#"{"font_size": 20.0, "new_tab_position": "middle", "notify_on_command_finish": "sometimes", "tab_bar_position": "diagonal"}"#,
        )
        .expect("a bad enum value must not fail the whole parse");
        assert_eq!(cfg.font_size, 20.0);
        assert_eq!(cfg.new_tab_position, NewTabPosition::AfterCurrent);
        assert_eq!(cfg.notify_on_command_finish, NotifyMode::Unfocused);
        assert_eq!(cfg.tab_bar_position, TabBarPosition::Left);

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
        assert_eq!(clamp(0.0), 1.0);
        assert_eq!(clamp(-3.0), 1.0);
        assert_eq!(clamp(100.0), 10.0);
        assert_eq!(clamp(0.01), 0.1);
    }

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
        assert_eq!(clamp(Some(0.0)), Some(0.2));
        assert_eq!(clamp(Some(2.0)), Some(1.0));
        assert_eq!(clamp(Some(f32::NAN)), None);
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
        assert_eq!(clamp(0), 100);
        assert_eq!(clamp(10_000), 10_000);
        assert_eq!(clamp(usize::MAX), MAX_SCROLLBACK);
    }

    #[test]
    fn new_terminal_prefs_default_and_parse_leniently() {
        let cfg = Config::default();
        assert!(cfg.restore_session);
        assert!(cfg.mouse_reporting);
        assert!(cfg.tab_completion);
        assert!(cfg.history_search);
        assert_eq!(cfg.notify_threshold_secs, 10);
        assert_eq!(cfg.bell, BellMode::Visual);

        let cfg: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        assert!(cfg.restore_session);
        assert!(cfg.mouse_reporting);
        assert!(cfg.tab_completion);
        assert!(cfg.history_search);
        assert_eq!(cfg.notify_threshold_secs, 10);
        assert_eq!(cfg.bell, BellMode::Visual);

        let cfg: Config = serde_json::from_str(r#"{"tab_completion": false}"#).unwrap();
        assert!(!cfg.tab_completion);
        let cfg: Config = serde_json::from_str(r#"{"history_search": false}"#).unwrap();
        assert!(!cfg.history_search);

        let cfg: Config = serde_json::from_str(
            r#"{"restore_session": false, "mouse_reporting": false, "bell": "audible"}"#,
        )
        .unwrap();
        assert!(!cfg.restore_session);
        assert!(!cfg.mouse_reporting);
        assert_eq!(cfg.bell, BellMode::Audible);

        let cfg: Config = serde_json::from_str(r#"{"bell": "both"}"#).unwrap();
        assert_eq!(cfg.bell, BellMode::Both);

        let cfg: Config = serde_json::from_str(r#"{"bell": "loud"}"#).unwrap();
        assert_eq!(cfg.bell, BellMode::Visual);

        assert_eq!(serde_json::to_string(&BellMode::Both).unwrap(), "\"both\"");
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
        assert_eq!(clamp(0), 1);
        assert_eq!(clamp(10), 10);
        assert_eq!(clamp(100_000), 3600);
    }

    #[test]
    fn sanitize_normalizes_blank_http_proxy_to_none() {
        let normalize = |proxy: Option<&str>| {
            let mut cfg = Config {
                http_proxy: proxy.map(String::from),
                ..Config::default()
            };
            cfg.sanitize();
            cfg.http_proxy
        };
        assert_eq!(normalize(None), None);
        assert_eq!(normalize(Some("")), None);
        assert_eq!(normalize(Some("   ")), None);
        assert_eq!(
            normalize(Some("  http://127.0.0.1:7890 ")),
            Some("http://127.0.0.1:7890".to_string())
        );
    }

    #[test]
    fn gui_language_defaults_to_english_and_rejects_unsupported_values() {
        let cfg = Config::default();
        assert_eq!(cfg.gui_language, "en");

        let cfg: Config = serde_json::from_str(r#"{"gui_language": "zh-CN"}"#).unwrap();
        assert_eq!(cfg.gui_language, "zh-CN");

        let cfg: Config = serde_json::from_str(r#"{"gui_language": "ja-JP"}"#).unwrap();
        assert_eq!(cfg.gui_language, "ja-JP");

        let mut cfg: Config = serde_json::from_str(r#"{"gui_language": "ko"}"#).unwrap();
        cfg.sanitize();
        assert_eq!(cfg.gui_language, "en");
    }

    #[test]
    fn keybinding_preset_and_prefix_default_and_round_trip() {
        let cfg = Config::default();
        assert_eq!(cfg.keybinding_preset, "default");
        assert_eq!(cfg.prefix, "ctrl-b");

        let cfg: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        assert_eq!(cfg.keybinding_preset, "default");
        assert_eq!(cfg.prefix, "ctrl-b");

        let cfg: Config =
            serde_json::from_str(r#"{"keybinding_preset": "tmux", "prefix": "ctrl-a"}"#).unwrap();
        assert_eq!(cfg.keybinding_preset, "tmux");
        assert_eq!(cfg.prefix, "ctrl-a");
    }

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

        let maple = defaults.iter().position(|f| f == "Maple Mono NF CN");
        let maple = maple.expect("the exact-fit CJK face must stay in the chain");
        for name in platform_last_resort_fallbacks() {
            let stock = defaults.iter().position(|f| f == name).unwrap();
            assert!(maple < stock, "{name} must not preempt Maple Mono NF CN");
        }

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
        let cfg: Config = serde_json::from_str(r#"{"font_size": 20.0}"#).unwrap();
        assert_eq!(cfg.font_size, 20.0);
        assert_eq!(cfg.line_height, 1.4);
        assert_eq!(cfg.font_family, "Hack");
        assert_eq!(cfg.theme_preset, "light");
        assert!(cfg.keybindings.is_empty());
    }

    fn pin_config_dir() {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        set_config_dir(dir);
    }

    static CONFIG_FILE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_config_file() -> std::sync::MutexGuard<'static, ()> {
        CONFIG_FILE.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn save_load_and_shell_command_round_trip_through_disk() {
        let _guard = lock_config_file();
        pin_config_dir();
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
        assert_eq!(loaded.ssh_profiles, vec![profile]);

        let (program, args) = shell_command().expect("shell override present");
        assert_eq!(program, "fish");
        assert_eq!(args, vec!["-l".to_string()]);
    }

    #[test]
    fn a_utf8_bom_does_not_silently_reset_the_config() {
        let _guard = lock_config_file();
        pin_config_dir();
        let path = Config::path().expect("pinned config dir");
        let text = "\u{FEFF}{\"font_size\": 21.0, \"restore_session\": false}";
        write_atomic(&path, text.as_bytes()).unwrap();

        let loaded = Config::load();
        assert_eq!(loaded.font_size, 21.0);
        assert!(!loaded.restore_session);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn strip_bom_only_removes_a_leading_marker() {
        assert_eq!(strip_bom("{}"), "{}");
        assert_eq!(strip_bom("\u{FEFF}{}"), "{}");
        assert_eq!(strip_bom("\u{FEFF}\u{FEFF}{}"), "\u{FEFF}{}");
        let inner = "{\"tab_title\":\"\u{FEFF}\"}";
        assert_eq!(strip_bom(inner), inner);
        assert_eq!(strip_bom(""), "");
    }

    #[test]
    fn ssh_profiles_default_empty_and_parse_from_json() {
        let cfg = Config::default();
        assert!(cfg.ssh_profiles.is_empty());
        let cfg: Config = serde_json::from_str(r#"{"font_size": 15.0}"#).unwrap();
        assert!(cfg.ssh_profiles.is_empty());

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
        assert_eq!(p.parent(), config_dir_path().as_deref());
    }
}

use gpui::{
    AnyElement, App, Background, Context, Div, ElementId, Entity, Focusable as _, FontWeight,
    KeyDownEvent, MouseButton, SharedString, Stateful, Subscription, Window, div, prelude::*, px,
    rgb,
};
use gpui_component::InteractiveElementExt as _;
use gpui_component::color_picker::{ColorPicker, ColorPickerState};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::link::Link;
use gpui_component::notification::{Notification, NotificationType};
use gpui_component::{Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use std::cell::{Cell, RefCell};

use uuid::Uuid;

use crate::core::config::{
    BellMode, Config, CursorStyle, LinkFileOpen, MouseZoomModifier, NewTabPosition, NotifyMode,
    TabBarPosition, UI_FONT_SIZE_DEFAULT, UpdateChannel, WindowBackdrop,
};
use crate::core::keychain::{
    CredentialRef, CredentialStore as _, OsCredentialStore, key_account_from_contents,
};
use crate::core::ssh_profile::{
    Algorithms, AuthMode, ForwardKind, ForwardRule, HostPort, SshProfile, to_connect_string,
};
use crate::ui::app::{
    FONT_SIZE_STEP, LINE_HEIGHT_STEP, TITLE_BAR_HEIGHT, ThemeEdit, Tty7App, UI_FONT_SIZE_STEP,
};
use crate::ui::host_ops::HostId;
use crate::ui::i18n::{L10nKey, t, t_fmt, t_plural};
use crate::ui::presets;

mod agents;
mod hosts;
pub(crate) mod kit;
mod pages;
mod shell;
mod shortcuts;
mod theme_picker;

/// The settings nav, and the page's padding either side of its column.
const NAV_W: f32 = 220.;
const PAGE_PAD: f32 = 112.;

/// The narrowest the nav is still itself: an item that still shows its label
/// beside its icon.
///
/// Sized for the longest nav label in *any* locale, not the one the developer
/// happens to be reading: at 140 the zh-CN "窗口与标签页" lost the right half
/// of its last character. The widest is ja-JP "ウィンドウとタブ".
const NAV_W_MIN: f32 = 176.;

/// The reading column every page caps itself at. Wider than this and a
/// description stops being a paragraph and becomes a line to scan across.
const READING_COLUMN: f32 = 620.;

/// How far the content scrollbar stays clear of the top and bottom of the
/// window, so it does not run into the window's rounded corner.
const SCROLLBAR_WINDOW_INSET: f32 = 12.;

/// What the page gets before the nav gives anything back: a row's widest
/// control with a description beside it that still reads as a paragraph.
const CONTENT_W: f32 = 420.;

/// How wide the nav is at this window width: its full width while the page
/// beside it still gets `CONTENT_W`, then shrinking toward its floor.
fn nav_width(viewport: f32, scale: f32) -> f32 {
    let full = NAV_W * scale.max(1.);
    let floor = NAV_W_MIN * scale.max(1.);
    let short = (full + CONTENT_W + PAGE_PAD - viewport).max(0.);
    (full - short).clamp(floor, full).round()
}

/// What a row on the page really has to lay out in.
fn settings_row_width(viewport: f32, scale: f32) -> f32 {
    (viewport - nav_width(viewport, scale) - PAGE_PAD).clamp(0., READING_COLUMN * scale)
}

/// How much wider every piece of text on this page is than the px thresholds
/// below assume.
///
/// Those thresholds are widths a *label* needs, measured at the default
/// interface font. The interface has a font size of its own and it goes up to
/// 24 — half as wide again — while a slider or a text field beside that label
/// stays the px width it was built at. Without this the row that has to stack
/// first is the one that never does.
fn ui_scale(cx: &App) -> f32 {
    cx.global::<Config>().ui_font_size / UI_FONT_SIZE_DEFAULT
}

/// How wide a text field in the right-hand column is.
const FIELD_W: f32 = 180.;

/// The host editor's label column, and how wide a field beside one grows to.
const SSH_LABEL_W: f32 = 96.;
const FORM_FIELD_W: f32 = 300.;

/// Width a settings row needs before its label and its control fit side by
/// side: a wide control, the gap between them, and enough left for a
/// description to read as prose rather than as a column of words.
const STACK_ROW_BELOW: f32 = 500.;

/// The scrollback presets, and the labels their cells carry, in draw order.
/// One list each so the number a cell writes is the number it shows —
/// `preset_row_labels_name_the_value_they_write` holds the two together.
const SCROLLBACK_BUCKETS: [usize; 3] = [1_000, 10_000, 100_000];
const SCROLLBACK_LABELS: [&str; 3] = ["1,000", "10,000", "100,000"];

/// The notify-threshold presets. The last one is drawn in minutes, which is
/// why these labels are written out rather than derived.
const NOTIFY_THRESHOLD_BUCKETS: [u64; 4] = [5, 10, 30, 60];
const NOTIFY_THRESHOLD_LABELS: [&str; 4] = ["5s", "10s", "30s", "1m"];

/// Which preset a live value *is*, and — when it is none of them — the label
/// for the trailing cell that names it.
///
/// The match is exact on purpose. Matching a *range* is what made
/// `scrollback_limit: 5000` light up "10,000" and `notify_threshold_secs: 20`
/// light up "30s", with no digits anywhere on the row to correct the
/// impression, and clicking the cell that was wrongly lit overwrote the real
/// value with the bucket's (#550).
fn preset_choice<T: Copy + PartialEq>(
    buckets: &[T],
    value: T,
    name: impl FnOnce(T) -> String,
) -> (Option<usize>, Option<String>) {
    match buckets.iter().position(|&b| b == value) {
        Some(ix) => (Some(ix), None),
        None => (
            None,
            Some(t_fmt(
                L10nKey::SettingsCustomValue,
                &[("value", &name(value))],
            )),
        ),
    }
}

/// `50000` beside cells reading `10,000` and `100,000` looks like a different
/// kind of number, so the custom cell groups its digits the way the presets
/// next to it are written. Every locale tty7 ships writes these counts the
/// same way — the preset labels themselves are one set of literals for all
/// three.
fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.char_indices() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn settings_row_id(label: &str, _desc: &str) -> SharedString {
    let id = settings_search_entries()
        .iter()
        .find(|entry| t(entry.title) == label)
        .map(|entry| format!("{:?}", entry.title))
        .or_else(|| {
            L10nKey::ALL
                .iter()
                .find(|&&key| t(key) == label)
                .map(|key| format!("{key:?}"))
        })
        .unwrap_or_else(|| label.to_string());
    SharedString::from(format!("settings-row-{id}"))
}

fn settings_header_id(title: &str) -> SharedString {
    let id = L10nKey::ALL
        .iter()
        .find(|&&key| t(key) == title)
        .map(|key| format!("{key:?}"))
        .unwrap_or_else(|| title.to_string());
    SharedString::from(format!("settings-header-{id}"))
}

/// Whether the reset control has any effective override to clear on this
/// platform. A synchronized Windows backdrop remains stored elsewhere but is
/// inert here, so only platforms that expose it locally may count it.
fn window_overrides_active(config: &Config, backdrop_is_local: bool) -> bool {
    config.window_opacity.is_some()
        || config.window_blur.is_some()
        || (backdrop_is_local && config.window_backdrop != WindowBackdrop::Auto)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    General,
    Appearance,
    Terminal,
    KeyboardMouse,
    Ssh,
    Agents,
    WindowTabs,
    Keybindings,
    About,
}

impl SettingsSection {
    pub(crate) const ALL: [SettingsSection; 8] = [
        SettingsSection::General,
        SettingsSection::Appearance,
        SettingsSection::Terminal,
        SettingsSection::KeyboardMouse,
        SettingsSection::WindowTabs,
        SettingsSection::Ssh,
        SettingsSection::Agents,
        SettingsSection::About,
    ];

    pub(crate) fn navigation_section(self) -> Self {
        match self {
            Self::Keybindings => Self::KeyboardMouse,
            other => other,
        }
    }

    fn title(self) -> L10nKey {
        match self {
            Self::General => L10nKey::SettingsNavGeneral,
            Self::Appearance => L10nKey::SettingsNavAppearance,
            Self::Terminal => L10nKey::SettingsNavTerminal,
            Self::KeyboardMouse => L10nKey::SettingsNavInput,
            Self::Ssh => L10nKey::SettingsNavSsh,
            Self::Agents => L10nKey::SettingsNavAgents,
            Self::WindowTabs => L10nKey::SettingsNavWindowTabs,
            Self::Keybindings => L10nKey::SettingsNavKeybindings,
            Self::About => L10nKey::SettingsNavAbout,
        }
    }

    fn icon_path(self) -> &'static str {
        match self {
            Self::General => "icons/settings/general.svg",
            Self::Appearance => "icons/settings/appearance.svg",
            Self::Terminal => "icons/settings/terminal.svg",
            Self::KeyboardMouse | Self::Keybindings => "icons/settings/keyboard.svg",
            Self::Ssh => "icons/settings/ssh.svg",
            Self::Agents => "icons/settings/integrations.svg",
            Self::WindowTabs => "icons/settings/window.svg",
            Self::About => "icons/settings/about.svg",
        }
    }

    fn profile_label(self) -> &'static str {
        match self {
            SettingsSection::General => "settings:general",
            SettingsSection::Appearance => "settings:appearance",
            SettingsSection::Terminal => "settings:terminal",
            SettingsSection::KeyboardMouse => "settings:keyboard-mouse",
            SettingsSection::Ssh => "settings:ssh",
            SettingsSection::Agents => "settings:agents",
            SettingsSection::WindowTabs => "settings:window-tabs",
            SettingsSection::Keybindings => "settings:keybindings",
            SettingsSection::About => "settings:about",
        }
    }
}

struct SearchEntry {
    section: SettingsSection,
    title: L10nKey,
    keywords: L10nKey,
}

use crate::core::update::{localized_update_install_hint, localized_update_phase};

fn settings_search_entries() -> &'static [SearchEntry] {
    use L10nKey::*;
    use SettingsSection::*;
    &[
        SearchEntry {
            section: Appearance,
            title: SettingsUiFontSize,
            keywords: SettingsSearchFontSizeKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsPerPaneHistory,
            keywords: SettingsSearchHistorySearchKeywords,
        },
        SearchEntry {
            section: KeyboardMouse,
            title: SettingsMouseZoom,
            keywords: SettingsSearchScrollSpeedKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsCustomPath,
            keywords: SettingsSearchStartInKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsOpenFilesCommand,
            keywords: SettingsSearchOpenFilesWithKeywords,
        },
        #[cfg(target_os = "macos")]
        SearchEntry {
            section: General,
            title: SettingsDefaultTerminal,
            keywords: SettingsSearchAboutKeywords,
        },
        SearchEntry {
            section: About,
            title: SettingsServer,
            keywords: SettingsSearchAboutKeywords,
        },
        SearchEntry {
            section: General,
            title: SettingsLanguage,
            keywords: SettingsSearchLanguageKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsThemeIntroTitle,
            keywords: SettingsSearchThemeKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsSyncWithSystem,
            keywords: SettingsSearchSyncWithSystemKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsLegiblePalette,
            keywords: SettingsSearchLegiblePaletteKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsCustomThemes,
            keywords: SettingsSearchCustomThemesKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsOpacity,
            keywords: SettingsSearchOpacityKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsBlur,
            keywords: SettingsSearchBlurKeywords,
        },
        #[cfg(target_os = "windows")]
        SearchEntry {
            section: Appearance,
            title: SettingsBackdrop,
            keywords: SettingsSearchBackdropKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsDimInactivePanes,
            keywords: SettingsSearchDimInactivePanesKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsFontSize,
            keywords: SettingsSearchFontSizeKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsUiFontFamily,
            keywords: SettingsSearchUiFontFamilyKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsLineHeight,
            keywords: SettingsSearchLineHeightKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsFontFamily,
            keywords: SettingsSearchFontFamilyKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsBoldFont,
            keywords: SettingsSearchBoldFontKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsItalicFont,
            keywords: SettingsSearchItalicFontKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsFontLigatures,
            keywords: SettingsSearchFontLigaturesKeywords,
        },
        #[cfg(target_os = "macos")]
        SearchEntry {
            section: Appearance,
            title: SettingsFontThicken,
            keywords: SettingsSearchFontThickenKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsCursorShape,
            keywords: SettingsSearchCursorShapeKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsCursorBlink,
            keywords: SettingsSearchCursorBlinkKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsAnsiColors,
            keywords: SettingsSearchAnsiColorsKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsBackgroundImage,
            keywords: SettingsSearchBackgroundImageKeywords,
        },
        SearchEntry {
            section: Appearance,
            title: SettingsImageOpacity,
            keywords: SettingsSearchImageOpacityKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsProgram,
            keywords: SettingsSearchProgramKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsArguments,
            keywords: SettingsSearchArgumentsKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsStartIn,
            keywords: SettingsSearchStartInKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsScrollback,
            keywords: SettingsSearchScrollbackKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsScrollSpeed,
            keywords: SettingsSearchScrollSpeedKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsSmoothScroll,
            keywords: SettingsSearchSmoothScrollKeywords,
        },
        SearchEntry {
            section: KeyboardMouse,
            title: SettingsFocusFollowsMouse,
            keywords: SettingsSearchFocusFollowsMouseKeywords,
        },
        SearchEntry {
            section: KeyboardMouse,
            title: SettingsHideMouseWhileTyping,
            keywords: SettingsSearchHideMouseWhileTypingKeywords,
        },
        SearchEntry {
            section: KeyboardMouse,
            title: SettingsReportMouseToApps,
            keywords: SettingsSearchReportMouseToAppsKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsTerminalBell,
            keywords: SettingsSearchTerminalBellKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: DetectUrls,
            keywords: SettingsSearchDetectUrlsKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: ForwardSshLoopbackLinks,
            keywords: SettingsSearchForwardSshLoopbackLinksKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: OpenFilesWith,
            keywords: SettingsSearchOpenFilesWithKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsPromptEditor,
            keywords: SettingsSearchPromptEditorKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsTabCompletion,
            keywords: SettingsSearchTabCompletionKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsHistorySearch,
            keywords: SettingsSearchHistorySearchKeywords,
        },
        #[cfg(target_os = "macos")]
        SearchEntry {
            section: KeyboardMouse,
            title: SettingsOptionAsMeta,
            keywords: SettingsSearchOptionAsMetaKeywords,
        },
        SearchEntry {
            section: KeyboardMouse,
            title: SettingsSmartSelection,
            keywords: SettingsSearchSmartSelectionKeywords,
        },
        SearchEntry {
            section: KeyboardMouse,
            title: SettingsCopyOnSelect,
            keywords: SettingsSearchCopyOnSelectKeywords,
        },
        SearchEntry {
            section: KeyboardMouse,
            title: SettingsTrimTrailingSpaces,
            keywords: SettingsSearchTrimTrailingSpacesKeywords,
        },
        SearchEntry {
            section: Ssh,
            title: SettingsHosts,
            keywords: SettingsSearchHostsKeywords,
        },
        SearchEntry {
            section: Ssh,
            title: SettingsVerifyHostKeys,
            keywords: SettingsSearchVerifyHostKeysKeywords,
        },
        SearchEntry {
            section: Ssh,
            title: WarnBeforeClosing,
            keywords: SettingsSearchWarnBeforeClosingKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentClaudeCode,
            keywords: SettingsSearchClaudeCodeKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentCodex,
            keywords: SettingsSearchCodexKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentTraeCode,
            keywords: SettingsSearchTraeCodeKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentCopilotCli,
            keywords: SettingsSearchCopilotCliKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentOpencode,
            keywords: SettingsSearchOpencodeKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentPi,
            keywords: SettingsSearchPiKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentGrokBuild,
            keywords: SettingsSearchGrokBuildKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentOhMyPi,
            keywords: SettingsSearchOhMyPiKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentGemini,
            keywords: SettingsSearchGeminiKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentDroid,
            keywords: SettingsSearchDroidKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentQwenCode,
            keywords: SettingsSearchQwenCodeKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentGoose,
            keywords: SettingsSearchGooseKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentKimiCode,
            keywords: SettingsSearchKimiCodeKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentQoderCLI,
            keywords: SettingsSearchQoderCLIKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentCrush,
            keywords: SettingsSearchCrushKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentCodeBuddy,
            keywords: SettingsSearchCodeBuddyKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsAgentCursorCli,
            keywords: SettingsSearchCursorCliKeywords,
        },
        SearchEntry {
            section: General,
            title: SettingsStartupWindow,
            keywords: SettingsSearchStartupWindowKeywords,
        },
        SearchEntry {
            section: General,
            title: SettingsRememberWindowSize,
            keywords: SettingsSearchRememberWindowSizeKeywords,
        },
        SearchEntry {
            section: General,
            title: SettingsRestoreLastLayout,
            keywords: SettingsSearchRestoreLastLayoutKeywords,
        },
        SearchEntry {
            section: General,
            title: SettingsShowTrayIcon,
            keywords: SettingsSearchShowTrayIconKeywords,
        },
        SearchEntry {
            section: WindowTabs,
            title: SettingsNewTabPosition,
            keywords: SettingsSearchNewTabPositionKeywords,
        },
        SearchEntry {
            section: WindowTabs,
            title: SettingsTabBarPosition,
            keywords: SettingsSearchTabBarPositionKeywords,
        },
        SearchEntry {
            section: WindowTabs,
            title: SettingsSidebarGrouping,
            keywords: SettingsSearchSidebarGroupingKeywords,
        },
        SearchEntry {
            section: WindowTabs,
            title: SettingsDiffPreviewFromCounts,
            keywords: SettingsSearchDiffPreviewFromCountsKeywords,
        },
        SearchEntry {
            section: General,
            title: SettingsNotifyOnCommandFinish,
            keywords: SettingsSearchNotifyOnCommandFinishKeywords,
        },
        SearchEntry {
            section: General,
            title: SettingsNotifyThreshold,
            keywords: SettingsSearchNotifyThresholdKeywords,
        },
        SearchEntry {
            section: KeyboardMouse,
            title: SettingsSearchKeybindingsTitle,
            keywords: SettingsSearchKeybindingsKeywords,
        },
        SearchEntry {
            section: About,
            title: SettingsNavAbout,
            keywords: SettingsSearchAboutKeywords,
        },
        SearchEntry {
            section: About,
            title: SettingsAppHttpProxy,
            keywords: SettingsSearchAppHttpProxyKeywords,
        },
        SearchEntry {
            section: About,
            title: SettingsUpdateChannel,
            keywords: SettingsSearchUpdateChannelKeywords,
        },
        SearchEntry {
            section: About,
            title: SettingsCheckUpdatesOnLaunch,
            keywords: SettingsSearchCheckUpdatesOnLaunchKeywords,
        },
        SearchEntry {
            section: About,
            title: SettingsAutoDownload,
            keywords: SettingsSearchAutoDownloadKeywords,
        },
        SearchEntry {
            section: Agents,
            title: SettingsInstallCliOnPath,
            keywords: SettingsSearchCommandLineToolKeywords,
        },
    ]
}

impl SearchEntry {
    fn config_key(&self) -> &'static str {
        match self.title {
            L10nKey::SettingsDimInactivePanes => "dim_inactive_panes",
            L10nKey::SettingsCursorBlink => "cursor_blink",
            L10nKey::SettingsCursorShape => "cursor_style",
            L10nKey::SettingsScrollback => "scrollback_limit",
            L10nKey::SettingsNewTabPosition => "new_tab_position",
            L10nKey::SettingsTabBarPosition => "tab_bar_position",
            L10nKey::SettingsSidebarGrouping => "sidebar_grouping",
            L10nKey::SettingsDiffPreviewFromCounts => "sidebar_diff_preview",
            L10nKey::SettingsNotifyOnCommandFinish => "notify_on_command_finish",
            L10nKey::SettingsNotifyThreshold => "notify_threshold_secs",
            L10nKey::SettingsTerminalBell => "bell",
            L10nKey::SettingsRestoreLastLayout => "restore_session",
            L10nKey::SettingsPerPaneHistory => "per_pane_history",
            L10nKey::SettingsShowTrayIcon => "show_tray_icon",
            L10nKey::SettingsOptionAsMeta => "macos_option_as_alt",
            L10nKey::SettingsHideMouseWhileTyping => "mouse_hide_while_typing",
            L10nKey::SettingsFocusFollowsMouse => "focus_follows_mouse",
            L10nKey::SettingsReportMouseToApps => "mouse_reporting",
            L10nKey::SettingsScrollSpeed => "mouse_scroll_multiplier",
            L10nKey::SettingsSmoothScroll => "smooth_scroll",
            L10nKey::SettingsMouseZoom => "mouse_zoom_modifier",
            L10nKey::SettingsTrimTrailingSpaces => "clipboard_trim_trailing_spaces",
            L10nKey::SettingsCopyOnSelect => "copy_on_select",
            L10nKey::SettingsSmartSelection => "smart_select",
            L10nKey::SettingsPromptEditor => "prompt_editor",
            L10nKey::SettingsTabCompletion => "tab_completion",
            L10nKey::SettingsHistorySearch => "history_search",
            L10nKey::SettingsStartupWindow => "startup_mode",
            L10nKey::SettingsRememberWindowSize => "remember_window_size",
            L10nKey::SettingsCheckUpdatesOnLaunch => "check_for_updates",
            L10nKey::SettingsAutoDownload => "auto_download_updates",
            L10nKey::SettingsUpdateChannel => "update_channel",
            L10nKey::DetectUrls => "link_url",
            L10nKey::ForwardSshLoopbackLinks => "ssh_loopback_forward",
            L10nKey::SettingsVerifyHostKeys => "verify_host_keys",
            L10nKey::WarnBeforeClosing => "ssh_warn_on_close",
            L10nKey::SettingsLanguage => "gui_language",
            L10nKey::SettingsProgram => "shell.program",
            L10nKey::SettingsArguments => "shell.args",
            L10nKey::SettingsStartIn => "working_directory.strategy",
            L10nKey::SettingsCustomPath => "working_directory.path",
            L10nKey::SettingsFontSize => "font_size",
            L10nKey::SettingsUiFontSize => "ui_font_size",
            L10nKey::SettingsLineHeight => "line_height",
            L10nKey::SettingsFontFamily => "font_family",
            L10nKey::SettingsBoldFont => "font_family_bold",
            L10nKey::SettingsItalicFont => "font_family_italic",
            L10nKey::SettingsUiFontFamily => "ui_font_family",
            L10nKey::SettingsFontLigatures => "font_features",
            L10nKey::SettingsFontThicken => "font_thicken",
            L10nKey::SettingsOpacity => "window_opacity",
            L10nKey::SettingsBlur => "window_blur",
            L10nKey::SettingsBackdrop => "window_backdrop",
            L10nKey::SettingsSyncWithSystem => "theme_follow_system",
            L10nKey::SettingsLegiblePalette => "theme_legible_palette",
            L10nKey::SettingsThemeIntroTitle => "theme_preset",
            L10nKey::SettingsAppHttpProxy => "http_proxy",
            L10nKey::SettingsOpenFilesCommand => "link_file_command",
            L10nKey::OpenFilesWith => "link_file_open",
            L10nKey::SettingsInstallCliOnPath => "install_cli_on_path",
            L10nKey::SettingsSearchKeybindingsTitle => "keybindings",
            L10nKey::SettingsHosts => "ssh_profiles",
            _ => "",
        }
    }
    fn description(&self) -> &'static str {
        match self.title {
            L10nKey::SettingsUiFontSize => t(L10nKey::SettingsUiFontSizeDesc),
            L10nKey::SettingsMouseZoom => t(L10nKey::SettingsMouseZoomDesc),
            L10nKey::SettingsCustomPath => t(L10nKey::SettingsCustomPathDesc),
            L10nKey::SettingsDefaultTerminal => t(L10nKey::SettingsDefaultTerminalDesc),
            L10nKey::SettingsServer => t(L10nKey::SettingsServerDesc),
            L10nKey::SettingsLanguage => t(L10nKey::SettingsLanguageDesc),
            L10nKey::SettingsSyncWithSystem => t(L10nKey::SettingsSyncWithSystemDesc),
            L10nKey::SettingsLegiblePalette => t(L10nKey::SettingsLegiblePaletteDesc),
            L10nKey::SettingsOpacity => t(L10nKey::SettingsOpacityDesc),
            L10nKey::SettingsBlur => t(L10nKey::SettingsBlurDesc),
            L10nKey::SettingsBackdrop => t(L10nKey::SettingsBackdropDesc),
            L10nKey::SettingsDimInactivePanes => t(L10nKey::SettingsDimInactivePanesDesc),
            L10nKey::SettingsFontSize => t(L10nKey::SettingsFontSizeDesc),
            L10nKey::SettingsUiFontFamily => t(L10nKey::SettingsUiFontFamilyDesc),
            L10nKey::SettingsLineHeight => t(L10nKey::SettingsLineHeightDesc),
            L10nKey::SettingsFontFamily => t(L10nKey::SettingsFontFamilyDesc),
            L10nKey::SettingsBoldFont => t(L10nKey::SettingsBoldFontDesc),
            L10nKey::SettingsItalicFont => t(L10nKey::SettingsItalicFontDesc),
            L10nKey::SettingsFontLigatures => t(L10nKey::SettingsFontLigaturesDesc),
            L10nKey::SettingsFontThicken => t(L10nKey::SettingsFontThickenDesc),
            L10nKey::SettingsCursorShape => t(L10nKey::SettingsCursorShapeDesc),
            L10nKey::SettingsCursorBlink => t(L10nKey::SettingsCursorBlinkDesc),
            L10nKey::SettingsBackgroundImage => t(L10nKey::SettingsBackgroundImageDesc),
            L10nKey::SettingsImageOpacity => t(L10nKey::SettingsImageOpacityDesc),
            L10nKey::SettingsProgram => t(L10nKey::SettingsProgramDesc),
            L10nKey::SettingsArguments => t(L10nKey::SettingsArgumentsDesc),
            L10nKey::SettingsStartIn => t(L10nKey::SettingsStartInDesc),
            L10nKey::SettingsScrollback => t(L10nKey::SettingsScrollbackDesc),
            L10nKey::SettingsScrollSpeed => t(L10nKey::SettingsScrollSpeedDesc),
            L10nKey::SettingsSmoothScroll => t(L10nKey::SettingsSmoothScrollDesc),
            L10nKey::SettingsFocusFollowsMouse => t(L10nKey::SettingsFocusFollowsMouseDesc),
            L10nKey::SettingsHideMouseWhileTyping => t(L10nKey::SettingsHideMouseWhileTypingDesc),
            L10nKey::SettingsReportMouseToApps => t(L10nKey::SettingsReportMouseToAppsDesc),
            L10nKey::SettingsTerminalBell => t(L10nKey::SettingsTerminalBellDesc),
            L10nKey::SettingsPromptEditor => t(L10nKey::SettingsPromptEditorDesc),
            L10nKey::SettingsTabCompletion => t(L10nKey::SettingsTabCompletionDesc),
            L10nKey::SettingsHistorySearch => t(L10nKey::SettingsHistorySearchDesc),
            L10nKey::SettingsOptionAsMeta => t(L10nKey::SettingsOptionAsMetaDesc),
            L10nKey::SettingsSmartSelection => t(L10nKey::SettingsSmartSelectionDesc),
            L10nKey::SettingsCopyOnSelect => t(L10nKey::SettingsCopyOnSelectDesc),
            L10nKey::SettingsTrimTrailingSpaces => t(L10nKey::SettingsTrimTrailingSpacesDesc),
            L10nKey::SettingsVerifyHostKeys => t(L10nKey::SettingsVerifyHostKeysDesc),
            L10nKey::SettingsStartupWindow => t(L10nKey::SettingsStartupWindowDesc),
            L10nKey::SettingsRememberWindowSize => t(L10nKey::SettingsRememberWindowSizeDesc),
            L10nKey::SettingsRestoreLastLayout => t(L10nKey::SettingsRestoreLastLayoutDesc),
            L10nKey::SettingsShowTrayIcon => t(L10nKey::SettingsShowTrayIconDesc),
            L10nKey::SettingsNewTabPosition => t(L10nKey::SettingsNewTabPositionDesc),
            L10nKey::SettingsTabBarPosition => t(L10nKey::SettingsTabBarPositionDesc),
            L10nKey::SettingsSidebarGrouping => t(L10nKey::SettingsSidebarGroupingDesc),
            L10nKey::SettingsDiffPreviewFromCounts => t(L10nKey::SettingsDiffPreviewFromCountsDesc),
            L10nKey::SettingsNotifyOnCommandFinish => t(L10nKey::SettingsNotifyOnCommandFinishDesc),
            L10nKey::SettingsNotifyThreshold => t(L10nKey::SettingsNotifyThresholdDesc),
            L10nKey::SettingsAppHttpProxy => t(L10nKey::SettingsAppHttpProxyDesc),
            L10nKey::SettingsUpdateChannel => t(L10nKey::SettingsUpdateChannelDesc),
            L10nKey::SettingsAutoDownload => t(L10nKey::SettingsAutoDownloadDesc),
            L10nKey::SettingsPerPaneHistory => t(L10nKey::SettingsPerPaneHistoryDescription),
            L10nKey::DetectUrls => t(L10nKey::SettingsDetectUrlsDesc),
            L10nKey::ForwardSshLoopbackLinks => t(L10nKey::SettingsForwardSshLoopbackLinksDesc),
            L10nKey::OpenFilesWith => t(L10nKey::SettingsOpenFilesModeDesc),
            _ => "",
        }
    }
    fn rank(&self, query: &str) -> u8 {
        let label = t(self.title).to_lowercase();
        let key = self.config_key();
        if label == query || key == query {
            0
        } else if label.starts_with(query) || (!key.is_empty() && key.starts_with(query)) {
            1
        } else if t(self.keywords)
            .split_whitespace()
            .any(|word| word.eq_ignore_ascii_case(query))
        {
            2
        } else {
            3
        }
    }
    fn modified(&self, cfg: &Config) -> bool {
        let defaults = Config::default();
        match self.title {
            L10nKey::SettingsDimInactivePanes => {
                cfg.dim_inactive_panes != defaults.dim_inactive_panes
            }
            L10nKey::SettingsCursorBlink => cfg.cursor_blink != defaults.cursor_blink,
            L10nKey::SettingsCursorShape => cfg.cursor_style != defaults.cursor_style,
            L10nKey::SettingsScrollback => cfg.scrollback_limit != defaults.scrollback_limit,
            L10nKey::SettingsNewTabPosition => cfg.new_tab_position != defaults.new_tab_position,
            L10nKey::SettingsTabBarPosition => cfg.tab_bar_position != defaults.tab_bar_position,
            L10nKey::SettingsSidebarGrouping => cfg.sidebar_grouping != defaults.sidebar_grouping,
            L10nKey::SettingsDiffPreviewFromCounts => {
                cfg.sidebar_diff_preview != defaults.sidebar_diff_preview
            }
            L10nKey::SettingsNotifyOnCommandFinish => {
                cfg.notify_on_command_finish != defaults.notify_on_command_finish
            }
            L10nKey::SettingsNotifyThreshold => {
                cfg.notify_threshold_secs != defaults.notify_threshold_secs
            }
            L10nKey::SettingsTerminalBell => cfg.bell != defaults.bell,
            L10nKey::SettingsRestoreLastLayout => cfg.restore_session != defaults.restore_session,
            L10nKey::SettingsPerPaneHistory => cfg.per_pane_history != defaults.per_pane_history,
            L10nKey::SettingsShowTrayIcon => cfg.show_tray_icon != defaults.show_tray_icon,
            L10nKey::SettingsOptionAsMeta => {
                cfg.macos_option_as_alt != defaults.macos_option_as_alt
            }
            L10nKey::SettingsHideMouseWhileTyping => {
                cfg.mouse_hide_while_typing != defaults.mouse_hide_while_typing
            }
            L10nKey::SettingsFocusFollowsMouse => {
                cfg.focus_follows_mouse != defaults.focus_follows_mouse
            }
            L10nKey::SettingsReportMouseToApps => cfg.mouse_reporting != defaults.mouse_reporting,
            L10nKey::SettingsScrollSpeed => {
                cfg.mouse_scroll_multiplier != defaults.mouse_scroll_multiplier
            }
            L10nKey::SettingsSmoothScroll => cfg.smooth_scroll != defaults.smooth_scroll,
            L10nKey::SettingsMouseZoom => cfg.mouse_zoom_modifier != defaults.mouse_zoom_modifier,
            L10nKey::SettingsTrimTrailingSpaces => {
                cfg.clipboard_trim_trailing_spaces != defaults.clipboard_trim_trailing_spaces
            }
            L10nKey::SettingsCopyOnSelect => cfg.copy_on_select != defaults.copy_on_select,
            L10nKey::SettingsSmartSelection => cfg.smart_select != defaults.smart_select,
            L10nKey::SettingsPromptEditor => cfg.prompt_editor != defaults.prompt_editor,
            L10nKey::SettingsTabCompletion => cfg.tab_completion != defaults.tab_completion,
            L10nKey::SettingsHistorySearch => cfg.history_search != defaults.history_search,
            L10nKey::SettingsStartupWindow => cfg.startup_mode != defaults.startup_mode,
            L10nKey::SettingsRememberWindowSize => {
                cfg.remember_window_size != defaults.remember_window_size
            }
            L10nKey::SettingsCheckUpdatesOnLaunch => {
                cfg.check_for_updates != defaults.check_for_updates
            }
            L10nKey::SettingsAutoDownload => {
                cfg.auto_download_updates != defaults.auto_download_updates
            }
            L10nKey::SettingsUpdateChannel => cfg.update_channel != defaults.update_channel,
            L10nKey::DetectUrls => cfg.link_url != defaults.link_url,
            L10nKey::ForwardSshLoopbackLinks => {
                cfg.ssh_loopback_forward != defaults.ssh_loopback_forward
            }
            L10nKey::SettingsVerifyHostKeys => cfg.verify_host_keys != defaults.verify_host_keys,
            L10nKey::WarnBeforeClosing => cfg.ssh_warn_on_close != defaults.ssh_warn_on_close,
            L10nKey::SettingsLanguage => cfg.gui_language != defaults.gui_language,
            L10nKey::SettingsFontSize => cfg.font_size != defaults.font_size,
            L10nKey::SettingsUiFontSize => cfg.ui_font_size != defaults.ui_font_size,
            L10nKey::SettingsLineHeight => cfg.line_height != defaults.line_height,
            L10nKey::SettingsFontFamily => cfg.font_family != defaults.font_family,
            L10nKey::SettingsBoldFont => cfg.font_family_bold != defaults.font_family_bold,
            L10nKey::SettingsItalicFont => cfg.font_family_italic != defaults.font_family_italic,
            L10nKey::SettingsUiFontFamily => cfg.ui_font_family != defaults.ui_font_family,
            L10nKey::SettingsOpacity => cfg.window_opacity != defaults.window_opacity,
            L10nKey::SettingsBlur => cfg.window_blur != defaults.window_blur,
            L10nKey::SettingsBackdrop => cfg.window_backdrop != defaults.window_backdrop,
            L10nKey::SettingsSyncWithSystem => {
                cfg.theme_follow_system != defaults.theme_follow_system
            }
            L10nKey::SettingsLegiblePalette => {
                cfg.theme_legible_palette != defaults.theme_legible_palette
            }
            L10nKey::SettingsThemeIntroTitle => {
                cfg.theme_preset != defaults.theme_preset
                    || cfg.theme_preset_light != defaults.theme_preset_light
                    || cfg.theme_preset_dark != defaults.theme_preset_dark
            }
            L10nKey::SettingsAppHttpProxy => cfg.http_proxy != defaults.http_proxy,
            L10nKey::SettingsOpenFilesCommand => {
                cfg.link_file_command != defaults.link_file_command
            }
            L10nKey::OpenFilesWith => cfg.link_file_open != defaults.link_file_open,
            L10nKey::SettingsSearchKeybindingsTitle => {
                cfg.keybindings != defaults.keybindings
                    || cfg.keybinding_preset != defaults.keybinding_preset
                    || cfg.prefix != defaults.prefix
            }
            L10nKey::SettingsProgram => {
                cfg.shell.as_ref().map(|s| &s.program)
                    != defaults.shell.as_ref().map(|s| &s.program)
            }
            L10nKey::SettingsArguments => cfg.shell.as_ref().is_some_and(|s| !s.args.is_empty()),
            L10nKey::SettingsStartIn => {
                cfg.working_directory.strategy != defaults.working_directory.strategy
            }
            L10nKey::SettingsCustomPath => {
                cfg.working_directory.path != defaults.working_directory.path
            }
            L10nKey::SettingsFontLigatures => cfg.font_features != defaults.font_features,
            L10nKey::SettingsFontThicken => cfg.font_thicken != defaults.font_thicken,
            _ => false,
        }
    }
}

fn entry_matches(entry: &SearchEntry, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.split_whitespace().all(|word| {
        t(entry.title).to_lowercase().contains(word)
            || match entry.title {
                L10nKey::SettingsMouseZoom => {
                    "zoom modifier scroll wheel 缩放 滚轮 修饰键 ズーム".contains(word)
                }
                L10nKey::SettingsPerPaneHistory => {
                    "per pane shell history independent 独立 命令历史".contains(word)
                }
                L10nKey::SettingsServer => {
                    "daemon server background service 后台 服务".contains(word)
                }
                _ => false,
            }
            || t(entry.keywords).to_lowercase().contains(word)
            || entry.description().to_lowercase().contains(word)
            || entry.config_key().contains(word)
            || crate::ui::i18n::alias_translations(entry.title)
                .iter()
                .any(|s| s.to_lowercase().contains(word))
            || crate::ui::i18n::alias_translations(entry.keywords)
                .iter()
                .any(|s| s.to_lowercase().contains(word))
    })
}

/// Whether one keybinding row answers the query.
///
/// The label is what the page shows and what someone searching for a feature
/// will type; the action name is what the docs and `keybindings.json` spell, so
/// `ScmSync` finds the row a reader arrived from the configuration page with.
pub(crate) fn keybinding_matches_query(action: &str, query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    let (_, label) = crate::ui::keymap::action_entry(action);
    label.to_lowercase().contains(query) || action.to_lowercase().contains(query)
}

/// The Keybindings page is the one page whose rows are not in the search index
/// above: there are eighty-odd of them, they are generated from the binding
/// table, and their labels are already localized there. Counting them here is
/// what puts an `(n)` on the nav item and lets `best_matching_section` land on
/// the page — without it, searching for a feature by name found the settings
/// that mention it and never the shortcut named exactly that (#444).
fn keybinding_match_count(query: &str) -> usize {
    if query.is_empty() {
        return 0;
    }
    crate::ui::keymap::default_bindings()
        .into_iter()
        .filter(|(action, _)| keybinding_matches_query(action, query))
        .count()
}

pub(crate) fn section_match_count(section: SettingsSection, query: &str) -> usize {
    let indexed = settings_search_entries()
        .iter()
        .filter(|e| e.section == section && entry_matches(e, query))
        .count();
    match section {
        SettingsSection::KeyboardMouse | SettingsSection::Keybindings => {
            indexed + keybinding_match_count(query)
        }
        _ => indexed,
    }
}

/// Whether a rendered row is one of the ones the section's `(n)` badge counted.
/// A row can match on its own label, or through the keyword list the search
/// index carries for it — "palette" finds "Theme" and nothing in that label
/// contains the word.
fn row_matches_query(section: SettingsSection, label: &str, query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    if label.to_lowercase().contains(query) {
        return true;
    }
    settings_search_entries()
        .iter()
        .any(|e| e.section == section && t(e.title) == label && entry_matches(e, query))
}

#[cfg(test)]
pub(crate) fn total_match_count(query: &str) -> usize {
    SettingsSection::ALL
        .into_iter()
        .map(|s| section_match_count(s, query))
        .sum()
}

#[cfg(test)]
pub(crate) fn best_matching_section(query: &str) -> Option<SettingsSection> {
    settings_search_entries()
        .iter()
        .filter(|entry| entry_matches(entry, query))
        .min_by_key(|entry| entry.rank(query))
        .map(|entry| entry.section)
        .or_else(|| (keybinding_match_count(query) > 0).then_some(SettingsSection::KeyboardMouse))
}

pub(crate) struct ThemeEditor {
    #[allow(dead_code)]
    pub(crate) for_id: String,
    pub(crate) seed: Vec<(ThemeEdit, Entity<ColorPickerState>)>,
    pub(crate) ansi: Vec<(ThemeEdit, Entity<ColorPickerState>)>,
    pub(crate) _subs: Vec<Subscription>,
}

pub(crate) struct SettingsState {
    pub(crate) focus_handle: gpui::FocusHandle,
    pub(crate) section: SettingsSection,
    pub(crate) search: Entity<InputState>,
    pub(crate) shortcut_search: Entity<InputState>,
    pub(crate) modified_only: bool,
    pub(crate) save_error: Option<String>,
    pub(crate) saved_config: Config,
    pub(crate) search_active: bool,
    pub(crate) search_return_offset: gpui::Point<gpui::Pixels>,
    pub(crate) search_selection: usize,
    pub(crate) search_rows: RefCell<Option<Vec<(L10nKey, AnyElement)>>>,
    pub(crate) focused_setting: Option<L10nKey>,
    /// The page's own scroll, and an anchor on it that the first matching row
    /// claims. Searching tells you "Appearance (2)"; these are what carry you
    /// to the two, which on a long page start well below the fold.
    pub(crate) content_scroll: gpui::ScrollHandle,
    pub(crate) search_anchor: gpui::ScrollAnchor,
    /// Set when the query or the section changes, and spent by the next render
    /// that has somewhere to go. A `Cell` because that render only holds `&self`.
    pub(crate) reveal_first_hit: Cell<bool>,
    /// The families the font menus list, read once when the page opens.
    pub(crate) font_names: std::rc::Rc<Vec<String>>,
    pub(crate) shell_program_input: Entity<InputState>,
    pub(crate) shell_args_input: Entity<InputState>,
    pub(crate) wd_path_input: Entity<InputState>,
    pub(crate) link_file_command_input: Entity<InputState>,
    pub(crate) http_proxy_input: Entity<InputState>,
    pub(crate) theme_editor: Option<ThemeEditor>,
    pub(crate) theme_draft: Option<(presets::Theme, presets::Theme)>,
    pub(crate) theme_draft_error: Option<String>,
    /// The one popover open on the page — a dropdown, a theme list, a row's
    /// menu — by the id its trigger was drawn with.
    pub(crate) menu: Option<SharedString>,
    /// The query box inside a popover that has one, and the row the arrow
    /// keys have reached in it.
    pub(crate) menu_query: Entity<InputState>,
    pub(crate) menu_hi: usize,
    pub(crate) menu_scroll: gpui::ScrollHandle,
    /// Whatever held keyboard focus when the page was last drawn, so a text
    /// field can draw its focus ring from a builder that has no `Window`.
    pub(crate) focused: RefCell<Option<gpui::FocusHandle>>,
    /// A recorded shortcut that another action already answers to, held until
    /// the user says whether to take it over.
    pub(crate) kb_conflict: Option<KeyConflict>,
    pub(crate) recording: Option<Recording>,
    pub(crate) rebinding_note: Option<String>,
    pub(crate) ssh_form: Option<SshProfileForm>,
    /// The host whose row is expanded to show its details.
    pub(crate) ssh_open: Option<Uuid>,
    /// Every host, grouped by where it is defined, rather than the recent few.
    pub(crate) ssh_show_all: bool,
    /// Remove was pressed once on the host being edited; the second press
    /// removes it.
    pub(crate) ssh_confirm_remove: bool,
    /// The host whose ssh command was just copied, for the moment the button
    /// says so.
    pub(crate) ssh_copied: Option<Uuid>,
    pub(crate) ssh_filter: Entity<InputState>,
    pub(crate) ssh_collapsed_groups: std::collections::HashSet<String>,
    pub(crate) agent_hooks_host: HostId,
    pub(crate) agent_hooks_states: AgentHooksView,
    pub(crate) agent_hooks_seq: u64,
    pub(crate) agent_hooks_note: Option<(crate::core::agent_hooks::HookAgent, String)>,
    pub(crate) agent_query: Entity<InputState>,
    /// Every agent, not just the ones installed on the selected machine.
    pub(crate) agent_show_all: bool,
    /// Agents acted on since the list was last narrowed, kept in view so a row
    /// does not vanish under the pointer the moment it is uninstalled.
    pub(crate) agent_touched: std::collections::HashSet<crate::core::agent_hooks::HookAgent>,
    /// Agents with an install or removal in flight.
    pub(crate) agent_busy: std::collections::HashSet<crate::core::agent_hooks::HookAgent>,
    pub(crate) _subs: Vec<Subscription>,
}

/// A recorded chord and the action it would be taken from.
pub(crate) struct KeyConflict {
    pub(crate) action: String,
    pub(crate) spec: String,
    pub(crate) other: String,
}

#[derive(Clone)]
pub(crate) enum AgentHooksView {
    Loading,
    Ready(Vec<AgentHookRow>),
    Unavailable(String),
}

#[derive(Clone)]
pub(crate) struct AgentHookRow {
    pub(crate) agent: crate::core::agent_hooks::HookAgent,
    pub(crate) state: crate::core::agent_hooks::HooksState,
    pub(crate) target: String,
}

#[derive(Clone)]
pub(crate) struct AgentHooksMachine {
    pub(crate) host: HostId,
    pub(crate) label: String,
}

/// The Appearance page's three ways of choosing a theme.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ThemeMode {
    Light,
    Dark,
    System,
}

fn ssh_group_key(p: &SshProfile) -> &str {
    p.group.as_deref().unwrap_or("")
}

fn ssh_group_label(key: &str) -> &str {
    match key {
        crate::core::ssh_config::IMPORTED_GROUP => "~/.ssh/config",
        "" => t(L10nKey::SettingsInTty7),
        other => other,
    }
}

fn ssh_group_rank(key: &str) -> u8 {
    match key {
        crate::core::ssh_config::IMPORTED_GROUP => 0,
        "" => 2,
        _ => 1,
    }
}

fn ssh_row_matches(p: &SshProfile, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let hit = |s: &str| s.to_lowercase().contains(query);
    hit(&p.name) || hit(&p.host) || hit(&p.user) || hit(&p.port.to_string())
}

/// How many *other* profiles reach the same `user@host:port` as this one.
///
/// The keychain is keyed by the endpoint, not by the profile, so two hosts that
/// differ only in how they get there — one direct, one through a jump host —
/// hand the same saved password back and forth. Every path that is about to
/// remove that password has to know this first: deleting a profile keeps the
/// secret while someone else still needs it, and forgetting one says out loud
/// who else it takes down. Both used to work the answer out on their own, which
/// is exactly how the two policies would have drifted apart.
fn profiles_sharing_endpoint(cfg: &Config, id: Uuid) -> usize {
    let Some(profile) = cfg.ssh_profiles.iter().find(|p| p.id == id) else {
        return 0;
    };
    cfg.ssh_profiles
        .iter()
        .filter(|p| {
            p.id != id && (&p.user, &p.host, p.port) == (&profile.user, &profile.host, profile.port)
        })
        .count()
}

pub(crate) struct SshProfileForm {
    editing: Uuid,
    carry_group: Option<String>,
    carry_credential_ref: Option<CredentialRef>,

    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    auth: AuthMode,

    /// The secret half of a connection. Neither of these is part of the
    /// profile — they live in the system keychain, and the config file holds
    /// no copy — so the form carries what the keychain had when it opened and
    /// compares against it on the way out. A form that was only read writes
    /// nothing back, and one that cleared a field says so.
    password: Entity<InputState>,
    passphrase: Entity<InputState>,
    loaded_password: String,
    loaded_passphrase: String,
    /// The endpoint the password above was read for, and the key file the
    /// passphrase belongs to. An edit to the address moves the entry, and
    /// without these there is nothing left pointing at the one to remove.
    loaded_endpoint: (String, String, u16),
    loaded_key: Option<String>,

    jump: Entity<InputState>,

    forwards: Vec<ForwardRuleForm>,

    identity_files: Entity<InputState>,
    proxy_command: Entity<InputState>,
    socks: Entity<InputState>,
    http: Entity<InputState>,
    kex: Entity<InputState>,
    cipher: Entity<InputState>,
    mac: Entity<InputState>,
    hostkey: Entity<InputState>,
    compression: Entity<InputState>,
    keepalive_interval: Entity<InputState>,
    keepalive_count: Entity<InputState>,
    connect_timeout: Entity<InputState>,
    login_scripts: Entity<InputState>,

    agent_forward: bool,
    x11: bool,
    skip_banner: bool,
    shell_integration: bool,
    remote_clipboard_write: bool,
    verify_host_keys: Option<bool>,
    warn_on_close: Option<bool>,

    _subs: Vec<Subscription>,
}

impl SshProfileForm {
    /// Whether the group that identifies the host — name, host, port, user —
    /// is still untouched. Every field notifies on change, so the form
    /// re-renders on each keystroke; without this a new host would be told it
    /// needs a host before anyone had the chance to type one. Same deal the
    /// forward rows strike with `ForwardRuleForm::is_blank`.
    fn core_is_blank(&self, cx: &App) -> bool {
        // The port is not in the list: it opens on 22 and is never empty, so
        // counting it meant a brand-new host was never "untouched" and the
        // form opened with "Needs a host" already in red under an empty box
        // nobody had reached yet.
        [&self.name, &self.host, &self.user]
            .iter()
            .all(|e| e.read(cx).value().trim().is_empty())
    }

    /// Whether either secret differs from what the keychain handed over.
    ///
    /// Nothing about a password reaches the profile, so the dirty check that
    /// compares profiles cannot see one being typed — without this, Save stays
    /// greyed out over a password the user just entered.
    ///
    /// Untrimmed on purpose: a trailing space is a character of the secret,
    /// and the server is the one that decides whether it belongs.
    fn secrets_changed(&self, cx: &App) -> bool {
        self.password.read(cx).value().as_ref() != self.loaded_password
            || self.passphrase.read(cx).value().as_ref() != self.loaded_passphrase
    }
}

/// What saving does to the keychain for the password field: which entry to
/// drop, and whether to write one.
///
/// A plain function because these are the cases a keychain makes expensive to
/// reach by hand — an address edited out from under a saved password, a field
/// cleared to mean "stop remembering this", a form opened and closed without a
/// keystroke.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PasswordPlan {
    drop_old: bool,
    store: bool,
}

fn password_plan(was: &str, typed: &str, moved: bool) -> PasswordPlan {
    PasswordPlan {
        // Only what this form read is ours to drop, and only once it is no
        // longer the entry this form would write to.
        drop_old: !was.is_empty() && (moved || typed.is_empty()),
        // A move rewrites even an unchanged secret: the account it is filed
        // under is the address, and the address is what changed.
        store: !typed.is_empty() && (typed != was || moved),
    }
}

pub(crate) struct ForwardRuleForm {
    pub(crate) kind: ForwardKind,
    pub(crate) bind_host: Entity<InputState>,
    pub(crate) bind_port: Entity<InputState>,
    pub(crate) target_host: Entity<InputState>,
    pub(crate) target_port: Entity<InputState>,
    pub(crate) description: Entity<InputState>,
}

impl ForwardRuleForm {
    fn collect(&self, cx: &App) -> Option<ForwardRule> {
        let val = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
        let bind_port: u16 = val(&self.bind_port).parse().ok().filter(|p| *p > 0)?;
        let bind = HostPort::new(val(&self.bind_host), bind_port);
        let target = if self.kind == ForwardKind::Dynamic {
            HostPort::default()
        } else {
            let port: u16 = val(&self.target_port).parse().ok().filter(|p| *p > 0)?;
            let host = val(&self.target_host);
            if host.is_empty() {
                return None;
            }
            HostPort::new(host, port)
        };
        Some(ForwardRule {
            kind: self.kind,
            bind,
            target,
            description: val(&self.description),
        })
    }
}

pub(crate) struct Recording {
    pub(crate) action: String,
    pub(crate) chords: Vec<String>,
    pub(crate) _intercept: Subscription,
}

pub(crate) fn font_default_label() -> &'static str {
    t(L10nKey::SettingsFontDefault)
}

/// The same first row for the interface face, spelled for what it actually
/// does. The bold and italic dropdowns fall back to the *terminal's* primary
/// family, which is what their label promises; the interface falls back to the
/// system UI font instead, so it cannot borrow that label without telling the
/// reader the chrome will come out in Hack.
pub(crate) fn ui_font_default_label() -> &'static str {
    t(L10nKey::SettingsUiFontDefault)
}

#[cfg(target_os = "macos")]
const LINK_MODIFIER_LABEL: &str = "⌘";
#[cfg(not(target_os = "macos"))]
const LINK_MODIFIER_LABEL: &str = "Ctrl";

pub(crate) fn humanize_action(action: &str) -> String {
    let mut out = String::new();
    for (i, ch) in action.chars().enumerate() {
        if i > 0 && ch.is_uppercase() {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// What a blank port field means. The same number `SshProfile`'s serde default
/// writes for a config that never mentioned a port, which is why leaving the
/// field empty has to stay legal: every host imported from `~/.ssh/config`
/// leaves it empty.
const DEFAULT_SSH_PORT: u16 = 22;

/// The port each proxy scheme listens on when the field names only a host.
const DEFAULT_SOCKS_PORT: u16 = 1080;
const DEFAULT_HTTP_PROXY_PORT: u16 = 8080;

/// A port as a form field spells it. Nothing here accepts 0: every port in a
/// profile is one something has to connect to, and no listener answers on 0.
fn parse_port(s: &str) -> Option<u16> {
    s.trim().parse::<u16>().ok().filter(|p| *p > 0)
}

/// A proxy address as the form spells it: blank is "no proxy", a bare host
/// takes the scheme's default port, and anything else has to carry a port that
/// exists. This used to be `parse().unwrap_or(0)`, so `proxy.example.com:88O`
/// saved a proxy on port 0 and the failure surfaced far away, in the socket
/// layer. The default port is not a secret either — `host_port_text` writes it
/// back into the field the next time the form opens.
fn parse_host_port_checked(s: &str, default_port: u16) -> Result<Option<HostPort>, SshFieldError> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    match s.rsplit_once(':') {
        Some((h, p)) => match parse_port(p) {
            Some(port) => Ok(Some(HostPort::new(h.trim(), port))),
            None => Err(SshFieldError::ProxyPortRange),
        },
        None => Ok(Some(HostPort::new(s, default_port))),
    }
}

fn host_port_text(hp: &Option<HostPort>) -> String {
    hp.as_ref()
        .map(|h| format!("{}:{}", h.host, h.port))
        .unwrap_or_default()
}

fn split_list(s: &str) -> Vec<String> {
    s.split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

fn split_lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Why one field of the SSH profile form cannot be saved. A value rather than
/// a finished sentence, so the rules stay a plain function a test can call —
/// the wording, and the locale it is written in, belong to the render pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SshFieldError {
    /// Nothing to connect to. Saved anyway, the profile used to render as an
    /// empty row in the host list and hand `TcpStream::connect` an empty name.
    HostMissing,
    /// A port field that is neither blank nor a port.
    PortRange,
    /// The same, for the port half of a proxy address.
    ProxyPortRange,
    /// The jump field names a profile no host list has.
    JumpUnknown(String),
    /// The jump field names the profile being edited.
    JumpIsSelf,
}

impl SshFieldError {
    fn message(&self) -> String {
        match self {
            Self::HostMissing => t(L10nKey::SettingsHostRequired).to_string(),
            Self::PortRange => t(L10nKey::SettingsPortInvalid).to_string(),
            Self::ProxyPortRange => t(L10nKey::SettingsProxyPortInvalid).to_string(),
            Self::JumpUnknown(name) => {
                t_fmt(L10nKey::SettingsJumpHostUnknown, &[("jump_name", name)])
            }
            Self::JumpIsSelf => t(L10nKey::SettingsJumpHostSelf).to_string(),
        }
    }
}

/// What the form has to fix before it can be saved, one slot per field so each
/// complaint can be printed under the control it is about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SshFormErrors {
    host: Option<SshFieldError>,
    port: Option<SshFieldError>,
    jump: Option<SshFieldError>,
    socks: Option<SshFieldError>,
    http: Option<SshFieldError>,
}

impl SshFormErrors {
    fn is_empty(&self) -> bool {
        self.host.is_none()
            && self.port.is_none()
            && self.jump.is_none()
            && self.socks.is_none()
            && self.http.is_none()
    }
}

/// The SSH profile form as plain text, lifted out of the `InputState` entities
/// it lives in. The rules that turn it into a profile are the part worth
/// testing, and a GPUI entity is not something a unit test can hand them, so
/// the window layer's job stops at reading the strings out.
#[derive(Debug, Clone, Default)]
pub(crate) struct SshFormDraft {
    id: Uuid,
    name: String,
    group: Option<String>,
    host: String,
    port: String,
    user: String,
    jump: String,
    proxy_command: String,
    socks: String,
    http: String,
    auth: AuthMode,
    identity_files: String,
    agent_forward: bool,
    credential_ref: Option<CredentialRef>,
    forwards: Vec<ForwardRule>,
    keepalive_interval: String,
    keepalive_count: String,
    connect_timeout: String,
    warn_on_close: Option<bool>,
    skip_banner: bool,
    shell_integration: bool,
    remote_clipboard_write: bool,
    login_scripts: String,
    x11: bool,
    kex: String,
    cipher: String,
    mac: String,
    hostkey: String,
    compression: String,
    verify_host_keys: Option<bool>,
}

/// The one place that decides what the form would save and what is wrong with
/// it. Both, always — never one or the other: the Escape prompt asks whether
/// the form differs from what is on disk, and a form that is merely invalid
/// still holds everything the user typed. Handing back only the errors would
/// make a brand-new invalid profile compare equal to the nothing on disk, and
/// Escape would throw the typing away without asking.
///
/// A missing `name` is deliberately not an error: the host list already falls
/// back to the host for a nameless profile, and requiring one would refuse
/// every host imported from `~/.ssh/config`.
fn validate_ssh_draft(draft: SshFormDraft, profiles: &[SshProfile]) -> (SshProfile, SshFormErrors) {
    let mut errors = SshFormErrors::default();

    let host = draft.host.trim().to_string();
    if host.is_empty() {
        errors.host = Some(SshFieldError::HostMissing);
    }

    let port_text = draft.port.trim();
    let port = match port_text.is_empty() {
        true => DEFAULT_SSH_PORT,
        false => parse_port(port_text).unwrap_or_else(|| {
            errors.port = Some(SshFieldError::PortRange);
            DEFAULT_SSH_PORT
        }),
    };

    // The field is a name but the profile stores an id, so a jump host already
    // survives its target being renamed. What it never survived was a name
    // nobody has: the lookup returned `None`, the profile saved as a direct
    // connection, and reopening the form showed an empty field.
    let jump_name = draft.jump.trim();
    let jump_host = if jump_name.is_empty() {
        None
    } else {
        let named = |p: &&SshProfile| p.name == jump_name;
        // Duplicate names resolve to whichever profile comes first, as they
        // always have. The one profile that can never be the answer is the one
        // being edited, and typing its own name is worth saying out loud
        // rather than quietly connecting direct.
        match profiles.iter().filter(named).find(|p| p.id != draft.id) {
            Some(p) => Some(p.id),
            None => {
                errors.jump = Some(match profiles.iter().any(|p| p.name == jump_name) {
                    true => SshFieldError::JumpIsSelf,
                    false => SshFieldError::JumpUnknown(jump_name.to_string()),
                });
                None
            }
        }
    };

    let proxy = |text: &str, default_port: u16, slot: &mut Option<SshFieldError>| {
        match parse_host_port_checked(text, default_port) {
            Ok(hp) => hp,
            Err(e) => {
                *slot = Some(e);
                None
            }
        }
    };
    let socks_proxy = proxy(&draft.socks, DEFAULT_SOCKS_PORT, &mut errors.socks);
    let http_proxy = proxy(&draft.http, DEFAULT_HTTP_PROXY_PORT, &mut errors.http);

    let proxy_command = draft.proxy_command.trim();
    let profile = SshProfile {
        id: draft.id,
        name: draft.name.trim().to_string(),
        group: draft.group,
        host,
        port,
        user: draft.user.trim().to_string(),
        jump_host,
        proxy_command: (!proxy_command.is_empty()).then(|| proxy_command.to_string()),
        socks_proxy,
        http_proxy,
        auth: draft.auth,
        identity_files: split_lines(&draft.identity_files),
        agent_forward: draft.agent_forward,
        credential_ref: draft.credential_ref,
        forwards: draft.forwards,
        keepalive_interval_s: draft.keepalive_interval.trim().parse().ok(),
        keepalive_count_max: draft.keepalive_count.trim().parse().ok(),
        connect_timeout_s: draft.connect_timeout.trim().parse().ok(),
        warn_on_close: draft.warn_on_close,
        skip_banner: draft.skip_banner,
        shell_integration: draft.shell_integration,
        remote_clipboard_write: draft.remote_clipboard_write,
        login_scripts: split_lines(&draft.login_scripts),
        x11: draft.x11,
        algorithms: Algorithms {
            kex: split_list(&draft.kex),
            cipher: split_list(&draft.cipher),
            mac: split_list(&draft.mac),
            hostkey: split_list(&draft.hostkey),
            compression: split_list(&draft.compression),
        },
        verify_host_keys: draft.verify_host_keys,
    };
    (profile, errors)
}

fn forward_row_inputs(row: &ForwardRuleForm) -> [&Entity<InputState>; 5] {
    [
        &row.bind_host,
        &row.bind_port,
        &row.target_host,
        &row.target_port,
        &row.description,
    ]
}

fn seed_forward_row(
    window: &mut Window,
    cx: &mut Context<Tty7App>,
    rule: &ForwardRule,
) -> ForwardRuleForm {
    let port = |p: u16| if p == 0 { String::new() } else { p.to_string() };
    ForwardRuleForm {
        kind: rule.kind,
        bind_host: seed_hinted(window, cx, &rule.bind.host, "localhost"),
        bind_port: seed_hinted(window, cx, &port(rule.bind.port), "8080"),
        target_host: seed_hinted(window, cx, &rule.target.host, "127.0.0.1"),
        target_port: seed_hinted(window, cx, &port(rule.target.port), "80"),
        description: seed_hinted(
            window,
            cx,
            &rule.description,
            t(L10nKey::ForwardDescriptionPlaceholder),
        ),
    }
}

fn seed_hinted(
    window: &mut Window,
    cx: &mut Context<Tty7App>,
    value: &str,
    placeholder: &'static str,
) -> Entity<InputState> {
    let value = value.to_string();
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
}

fn seed_input(
    window: &mut Window,
    cx: &mut Context<Tty7App>,
    value: &str,
    multi_line: bool,
) -> Entity<InputState> {
    let value = value.to_string();
    cx.new(|cx| {
        InputState::new(window, cx)
            .multi_line(multi_line)
            .default_value(value)
    })
}

/// A picked path written the way a config file spells it. `~/.ssh/id_ed25519`
/// keeps meaning the right file on another machine, or after the account is
/// renamed; the absolute path the system picker hands back does not.
fn tildify(path: &str) -> String {
    #[cfg(windows)]
    let home = std::env::var("USERPROFILE").ok();
    #[cfg(not(windows))]
    let home = std::env::var("HOME").ok();
    tildify_with(path, home.as_deref().filter(|h| !h.is_empty()))
}

fn tildify_with(path: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return path.to_string();
    };
    let home = home.trim_end_matches(['/', '\\']);
    match path.strip_prefix(home) {
        // A separator has to follow, or `/Users/adalovelace` would come back
        // as a file inside `/Users/ada`.
        Some(rest) if rest.starts_with('/') || rest.starts_with('\\') => format!(
            "~/{}",
            rest.trim_start_matches(['/', '\\']).replace('\\', "/")
        ),
        _ => path.to_string(),
    }
}

/// What the key field shows while it is empty: the file ssh would reach for on
/// its own. A hint, not a value — an empty field still means "try the usual
/// `~/.ssh` keys", which is exactly what `default_identity_candidates` does.
const DEFAULT_KEY_HINT: &str = "~/.ssh/id_ed25519";

/// The password the keychain holds for this profile's endpoint, or nothing.
///
/// Read once, when a host is opened for editing — not per render, and not per
/// keystroke. A profile with no host yet has no endpoint to ask about: the
/// account would come out as `@:22`, which belongs to no server.
fn stored_password(profile: &SshProfile) -> String {
    if profile.host.trim().is_empty() {
        return String::new();
    }
    OsCredentialStore
        .password_for(&profile.user, &profile.host, profile.port)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// The first key file a profile would offer that is actually there.
///
/// A passphrase is accounted by the key's *contents*, not by its path, so a
/// file that cannot be read is a key nothing can be stored against.
fn first_readable_key(profile: &SshProfile) -> Option<String> {
    first_readable_key_in(&profile.identity_files, &profile.host, &profile.user)
}

/// The same answer for a form that has not been collected into a profile yet:
/// the key field as typed, with the host and user beside it filling in `%h`
/// and `%r`.
fn first_readable_key_in(files: &[String], host: &str, user: &str) -> Option<String> {
    first_readable_key_or(
        files,
        host,
        user,
        crate::core::ssh_profile::default_identity_candidates,
    )
}

/// An empty key field is not "no key": `build_spec_inner` offers the `~/.ssh`
/// defaults then, and looks their passphrases up by those exact strings. The
/// box has to follow the same list, or the most common setup — no key named,
/// an encrypted `id_ed25519` — could never be given a passphrase here.
fn first_readable_key_or(
    files: &[String],
    host: &str,
    user: &str,
    defaults: impl FnOnce() -> Vec<String>,
) -> Option<String> {
    let candidates = if files.is_empty() {
        defaults()
    } else {
        files
            .iter()
            .map(|f| crate::core::ssh_profile::expand_identity_placeholders(f, host, user))
            .collect()
    };
    candidates
        .into_iter()
        .find(|p| std::fs::metadata(p).is_ok())
}

fn stored_passphrase(key_path: &str) -> String {
    let path = crate::core::ssh_profile::expand_tilde(key_path);
    let Ok(bytes) = std::fs::read(&path) else {
        return String::new();
    };
    OsCredentialStore
        .passphrase_for_key(&key_account_from_contents(&bytes))
        .ok()
        .flatten()
        .unwrap_or_default()
}

impl Tty7App {
    pub(crate) fn with_settings_edits_resolved(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        action: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let save_failed = self
            .active_settings()
            .is_some_and(|s| s.save_error.is_some());
        if !save_failed && !self.ssh_form_dirty(cx) && !self.theme_draft_dirty() {
            action(self, window, cx);
            return;
        }
        let answer = window.prompt::<gpui::PromptButton>(
            gpui::PromptLevel::Warning,
            t(L10nKey::SettingsUnsavedTitle),
            Some(t(L10nKey::SettingsUnsavedBody)),
            &[
                gpui::PromptButton::ok(t(L10nKey::SettingsSaveChanges)),
                gpui::PromptButton::new(t(L10nKey::EditorDiscard)),
                gpui::PromptButton::cancel(t(L10nKey::SettingsKeepEditing)),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(choice) = answer.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                match choice {
                    0 => {
                        if this
                            .active_settings()
                            .is_some_and(|s| s.save_error.is_some())
                        {
                            this.persist_settings_config(cx);
                            if this
                                .active_settings()
                                .is_some_and(|s| s.save_error.is_some())
                            {
                                return;
                            }
                        }
                        if this.ssh_form_dirty(cx)
                            && this.save_editing_profile(window, cx).is_none()
                        {
                            return;
                        }
                        if !this.save_theme_draft(window, cx) {
                            return;
                        }
                    }
                    1 => {
                        this.discard_unsaved_settings(window, cx);
                        this.cancel_theme_draft(window, cx);
                        if let Some(s) = this.active_settings_mut() {
                            s.ssh_form = None;
                        }
                    }
                    _ => return,
                }
                action(this, window, cx);
            });
        })
        .detach();
    }

    pub(crate) fn navigate_settings(
        &mut self,
        target: SettingsSection,
        setting: Option<L10nKey>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ssh_form_dirty(cx)
            || self.theme_draft_dirty()
            || self
                .active_settings()
                .is_some_and(|s| s.save_error.is_some())
        {
            self.with_settings_edits_resolved(window, cx, move |this, window, cx| {
                this.navigate_settings(target, setting, window, cx)
            });
            return;
        }
        if let Some(state) = self.active_settings() {
            state
                .search
                .clone()
                .update(cx, |search, cx| search.set_value("", window, cx));
        }
        let setting = match setting {
            Some(L10nKey::SettingsCustomPath)
                if cx.global::<Config>().working_directory.strategy
                    != crate::core::config::WdStrategy::Custom =>
            {
                Some(L10nKey::SettingsStartIn)
            }
            Some(L10nKey::SettingsOpenFilesCommand)
                if cx.global::<Config>().file_open_mode() != LinkFileOpen::Command =>
            {
                Some(L10nKey::OpenFilesWith)
            }
            other => other,
        };
        if let Some(state) = self.active_settings_mut() {
            state.modified_only = false;
            state.search_active = false;
            state.focused_setting = setting;
            state.reveal_first_hit.set(setting.is_some());
            state.content_scroll.set_offset(gpui::point(px(0.), px(0.)));
            state.menu = None;
        }
        self.select_settings_section(target, cx);
    }

    /// Whether the row measured this render came out narrower than a threshold
    /// quoted at the default interface font — the only way those px thresholds
    /// mean anything to a reader who scaled the interface up.
    fn settings_row_under(&self, at_default_font: f32, cx: &App) -> bool {
        self.settings_row_width.get() < at_default_font * ui_scale(cx)
    }

    /// The scroll anchor for the first thing on the page the query matched,
    /// whatever kind of element that is. A section header can be the only
    /// match on its page — "ansi", "how shells work" — and while the dimming
    /// around it already picks it out, nothing was carrying the page to it:
    /// search "ansi" from the bottom of Appearance and every row greys out
    /// with the one answer left above the fold.
    fn first_hit_anchor(&self, label: &str, cx: &Context<Self>) -> Option<gpui::ScrollAnchor> {
        let s = self.active_settings()?;
        if s.search_active || s.search_rows.borrow().is_some() {
            return None;
        }
        if let Some(target) = s.focused_setting {
            if t(target) == label && !self.settings_hit_anchored.replace(true) {
                return Some(s.search_anchor.clone());
            }
            return None;
        }
        let query = s.search.read(cx).value().trim().to_lowercase();
        if query.is_empty() || section_match_count(s.section, &query) == 0 {
            return None;
        }
        if !row_matches_query(s.section, label, &query) {
            return None;
        }
        match self.settings_hit_anchored.replace(true) {
            false => Some(s.search_anchor.clone()),
            true => None,
        }
    }

    fn live_ssh_profiles(&self, cx: &App) -> std::collections::HashSet<Uuid> {
        use crate::daemon::protocol::SshPhase;
        let mut live = std::collections::HashSet::new();
        for tab in &self.tabs {
            for leaf in tab.pane.terminals() {
                let v = leaf.read(cx);
                if !matches!(v.ssh_phase(), Some(SshPhase::Connected)) || v.terminal.exited {
                    continue;
                }
                if let Some(id) = v
                    .ssh_spec()
                    .and_then(|s| s.profile_id.clone())
                    .and_then(|id| Uuid::parse_str(&id).ok())
                {
                    live.insert(id);
                }
            }
        }
        live
    }

    fn toggle_ssh_group(&mut self, key: String, cx: &mut Context<Self>) {
        if let Some(s) = self.active_settings_mut() {
            if !s.ssh_collapsed_groups.remove(&key) {
                s.ssh_collapsed_groups.insert(key);
            }
        }
        // Collapsing the list never discards the profile being edited.
        cx.notify();
    }

    fn ssh_form_mut(&mut self) -> Option<&mut SshProfileForm> {
        self.active_settings_mut().and_then(|s| s.ssh_form.as_mut())
    }

    pub(crate) fn ssh_form_load(
        &mut self,
        profile: &SshProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ssh_form_dirty(cx) {
            let profile = profile.clone();
            self.with_settings_edits_resolved(window, cx, move |this, window, cx| {
                this.ssh_form_load(&profile, window, cx)
            });
            return;
        }
        let jump_name = profile
            .jump_host
            .and_then(|id| {
                cx.global::<Config>()
                    .ssh_profiles
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.name.clone())
            })
            .unwrap_or_default();

        let name = seed_hinted(window, cx, &profile.name, t(L10nKey::SettingsNameHint));
        let host = seed_hinted(window, cx, &profile.host, t(L10nKey::SettingsHostHint));
        let port = seed_input(window, cx, &profile.port.to_string(), false);
        let user = seed_hinted(window, cx, &profile.user, t(L10nKey::SettingsUserHint));
        let jump = seed_input(window, cx, &jump_name, false);
        let forwards: Vec<ForwardRuleForm> = profile
            .forwards
            .iter()
            .map(|r| seed_forward_row(window, cx, r))
            .collect();
        // One line, comma-separated: the form is a row of single-line fields,
        // and a profile with more than one key is the exception.
        let identity_files = seed_hinted(
            window,
            cx,
            &profile.identity_files.join(", "),
            DEFAULT_KEY_HINT,
        );

        // One keychain read per host opened, not one per keystroke: the
        // password box shows what is actually stored, the way every other SSH
        // client shows it, so it can be read back, corrected or cleared
        // without connecting first.
        let loaded_password = stored_password(profile);
        let loaded_key = first_readable_key(profile);
        let loaded_passphrase = loaded_key
            .as_deref()
            .map(stored_passphrase)
            .unwrap_or_default();
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t(L10nKey::SettingsPasswordHint))
                .default_value(loaded_password.clone())
        });
        let passphrase = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t(L10nKey::SettingsPasswordHint))
                .default_value(loaded_passphrase.clone())
        });
        let proxy_command = seed_input(
            window,
            cx,
            profile.proxy_command.as_deref().unwrap_or(""),
            false,
        );
        let socks = seed_input(window, cx, &host_port_text(&profile.socks_proxy), false);
        let http = seed_input(window, cx, &host_port_text(&profile.http_proxy), false);
        let kex = seed_input(window, cx, &profile.algorithms.kex.join(", "), false);
        let cipher = seed_input(window, cx, &profile.algorithms.cipher.join(", "), false);
        let mac = seed_input(window, cx, &profile.algorithms.mac.join(", "), false);
        let hostkey = seed_input(window, cx, &profile.algorithms.hostkey.join(", "), false);
        let compression = seed_input(
            window,
            cx,
            &profile.algorithms.compression.join(", "),
            false,
        );
        let keepalive_interval = seed_input(
            window,
            cx,
            &profile
                .keepalive_interval_s
                .map(|n| n.to_string())
                .unwrap_or_default(),
            false,
        );
        let keepalive_count = seed_input(
            window,
            cx,
            &profile
                .keepalive_count_max
                .map(|n| n.to_string())
                .unwrap_or_default(),
            false,
        );
        let connect_timeout = seed_input(
            window,
            cx,
            &profile
                .connect_timeout_s
                .map(|n| n.to_string())
                .unwrap_or_default(),
            false,
        );
        let login_scripts = seed_input(window, cx, &profile.login_scripts.join("\n"), true);

        let mut subs = Vec::new();
        // The passphrase belongs to whichever key the field above names, so
        // when that answer changes the box has to change with it. Without this
        // a form opened on one key and pointed at another would carry the
        // first key's passphrase across and save it over the second's. Host and
        // user count too: they fill in a `%h` / `%r` in the key's path.
        for input in [&identity_files, &host, &user] {
            subs.push(
                cx.subscribe_in(input, window, |this, _i, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::Change) {
                        this.resync_key_passphrase(window, cx);
                    }
                }),
            );
        }
        let mut watch = vec![
            &name,
            &host,
            &port,
            &user,
            &jump,
            &password,
            &passphrase,
            &identity_files,
            &proxy_command,
            &socks,
            &http,
            &kex,
            &cipher,
            &mac,
            &hostkey,
            &compression,
            &keepalive_interval,
            &keepalive_count,
            &connect_timeout,
            &login_scripts,
        ];
        for row in &forwards {
            watch.extend(forward_row_inputs(row));
        }
        for input in watch {
            subs.push(
                cx.subscribe_in(input, window, |this, _i, ev: &InputEvent, _w, cx| {
                    if matches!(ev, InputEvent::Change) {
                        let _ = this.ssh_form_mut();
                        cx.notify();
                    }
                }),
            );
        }

        let form = SshProfileForm {
            editing: profile.id,
            carry_group: profile.group.clone(),
            carry_credential_ref: profile.credential_ref.clone(),
            name,
            host,
            port,
            user,
            auth: profile.auth,
            password,
            passphrase,
            loaded_password,
            loaded_passphrase,
            loaded_endpoint: (profile.user.clone(), profile.host.clone(), profile.port),
            loaded_key,
            jump,
            forwards,
            identity_files,
            proxy_command,
            socks,
            http,
            kex,
            cipher,
            mac,
            hostkey,
            compression,
            keepalive_interval,
            keepalive_count,
            connect_timeout,
            login_scripts,
            agent_forward: profile.agent_forward,
            x11: profile.x11,
            skip_banner: profile.skip_banner,
            shell_integration: profile.shell_integration,
            remote_clipboard_write: profile.remote_clipboard_write,
            verify_host_keys: profile.verify_host_keys,
            warn_on_close: profile.warn_on_close,
            _subs: subs,
        };
        let editing = form.editing;
        let known = cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .any(|p| p.id == editing);
        if let Some(s) = self.active_settings_mut() {
            s.ssh_form = Some(form);
            s.ssh_confirm_remove = false;
            // A saved host edits in its own row; a new one gets a row of its
            // own at the top, so nothing else is shown opened.
            s.ssh_open = known.then_some(editing);
        }
        cx.notify();
    }

    /// Reads the form out of its entities and runs it past
    /// [`validate_ssh_draft`]. The profile that comes back is what the form
    /// would save; the errors are what stands in the way.
    fn ssh_form_collect(&self, cx: &App) -> Option<(SshProfile, SshFormErrors)> {
        let form = self.active_settings()?.ssh_form.as_ref()?;
        let val = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
        // The multi-line and comma-separated fields do their own splitting, so
        // they travel whole rather than trimmed.
        let raw = |e: &Entity<InputState>| e.read(cx).value().to_string();

        let draft = SshFormDraft {
            id: form.editing,
            name: val(&form.name),
            group: form.carry_group.clone(),
            host: val(&form.host),
            port: val(&form.port),
            user: val(&form.user),
            jump: val(&form.jump),
            proxy_command: val(&form.proxy_command),
            socks: val(&form.socks),
            http: val(&form.http),
            auth: form.auth,
            identity_files: raw(&form.identity_files).replace(',', "\n"),
            agent_forward: form.agent_forward,
            credential_ref: form.carry_credential_ref.clone(),
            forwards: form.forwards.iter().filter_map(|r| r.collect(cx)).collect(),
            keepalive_interval: val(&form.keepalive_interval),
            keepalive_count: val(&form.keepalive_count),
            connect_timeout: val(&form.connect_timeout),
            warn_on_close: form.warn_on_close,
            skip_banner: form.skip_banner,
            shell_integration: form.shell_integration,
            remote_clipboard_write: form.remote_clipboard_write,
            login_scripts: raw(&form.login_scripts),
            x11: form.x11,
            kex: raw(&form.kex),
            cipher: raw(&form.cipher),
            mac: raw(&form.mac),
            hostkey: raw(&form.hostkey),
            compression: raw(&form.compression),
            verify_host_keys: form.verify_host_keys,
        };
        Some(validate_ssh_draft(
            draft,
            &cx.global::<Config>().ssh_profiles,
        ))
    }

    /// Point the passphrase box at the key the form now names.
    ///
    /// A passphrase is stored against the contents of the key it unlocks, so
    /// the box is only ever right about one key at a time. A box the user has
    /// started typing in is left alone — it is the one place where what is on
    /// screen outranks what the keychain holds.
    fn resync_key_passphrase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.active_settings().and_then(|s| s.ssh_form.as_ref()) else {
            return;
        };
        let files = split_lines(&form.identity_files.read(cx).value().replace(',', "\n"));
        let host = form.host.read(cx).value().trim().to_string();
        let user = form.user.read(cx).value().trim().to_string();
        let key = first_readable_key_in(&files, &host, &user);
        if key == form.loaded_key {
            return;
        }
        let stored = key.as_deref().map(stored_passphrase).unwrap_or_default();
        // A box the user has already typed in keeps what they typed — only
        // the key it will be saved against moves under it.
        let untouched = form.passphrase.read(cx).value().as_ref() == form.loaded_passphrase;
        let input = form.passphrase.clone();
        if let Some(form) = self.ssh_form_mut() {
            form.loaded_key = key;
            form.loaded_passphrase = stored.clone();
        }
        if untouched {
            input.update(cx, |i, cx| i.set_value(stored, window, cx));
        }
        cx.notify();
    }

    pub(crate) fn save_editing_profile(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Uuid> {
        let (profile, errors) = self.ssh_form_collect(cx)?;
        // Save and Connect are both disabled while anything is wrong, but this
        // is the door all of them go through, and what gets past it lands in
        // the config file — where a host-less profile is a blank row nobody
        // can identify or delete on sight.
        if !errors.is_empty() {
            return None;
        }
        let id = profile.id;
        self.update_config(cx, |cfg| {
            if let Some(slot) = cfg.ssh_profiles.iter_mut().find(|p| p.id == id) {
                *slot = profile.clone();
            } else {
                cfg.ssh_profiles.push(profile.clone());
            }
        });
        if self
            .active_settings()
            .is_some_and(|s| s.save_error.is_some())
        {
            return None;
        }
        self.save_ssh_form_secrets(&profile, window, cx);
        Some(id)
    }

    pub(crate) fn cancel_ssh_form(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.active_settings_mut() {
            s.ssh_form = None;
            s.ssh_confirm_remove = false;
        }
        cx.notify();
    }

    /// Move the two secrets in the form into the keychain — or out of it.
    ///
    /// The config file never holds either of them, so this is the whole of
    /// what saving means for a password: an entry keyed by the endpoint the
    /// profile now names. Editing the address moves the entry rather than
    /// leaving the old one behind to be offered to a host that no longer
    /// exists, and clearing the field removes it.
    fn save_ssh_form_secrets(
        &mut self,
        profile: &SshProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = self.active_settings().and_then(|s| s.ssh_form.as_ref()) else {
            return;
        };
        let typed = form.password.read(cx).value().to_string();
        let typed_passphrase = form.passphrase.read(cx).value().to_string();
        let (old_user, old_host, old_port) = form.loaded_endpoint.clone();
        let was = form.loaded_password.clone();
        let was_passphrase = form.loaded_passphrase.clone();
        let moved = (old_user.as_str(), old_host.as_str(), old_port)
            != (profile.user.as_str(), profile.host.as_str(), profile.port);

        let mut failures: Vec<String> = Vec::new();
        let endpoint = |u: &str, h: &str, p: u16| format!("{u}@{h}:{p}");
        let plan = password_plan(&was, &typed, moved);

        // The entry the form read from, once it is no longer the entry the
        // form would write to. Left alone when another profile still dials the
        // same address — the keychain accounts by endpoint, not by profile.
        let stranded = plan.drop_old && !old_host.trim().is_empty();
        if stranded
            && !self.endpoint_still_in_use(&old_user, &old_host, old_port, profile.id, cx)
            && let Err(e) = OsCredentialStore.delete_password(&old_user, &old_host, old_port)
        {
            failures.push(t_fmt(
                L10nKey::SettingsCouldntForgetPassword,
                &[
                    ("endpoint", &endpoint(&old_user, &old_host, old_port)),
                    ("error", &e.to_string()),
                ],
            ));
        }
        if plan.store
            && !profile.host.trim().is_empty()
            && let Err(e) =
                OsCredentialStore.set_password(&profile.user, &profile.host, profile.port, &typed)
        {
            failures.push(t_fmt(
                L10nKey::SettingsCouldntSavePassword,
                &[
                    (
                        "endpoint",
                        &endpoint(&profile.user, &profile.host, profile.port),
                    ),
                    ("error", &e.to_string()),
                ],
            ));
        }

        // A passphrase belongs to the key it unlocks, not to this profile, so
        // a save only ever touches the entry for the key named here. Pointing
        // the profile at a different key leaves the first key's passphrase
        // alone — other hosts use that key too.
        let key = first_readable_key(profile);
        if typed_passphrase != was_passphrase {
            match key.as_deref() {
                Some(path) => self.write_key_passphrase(path, &typed_passphrase, &mut failures),
                // Nowhere to put it: the field names no key, or names one that
                // is not on this machine. Storing nothing quietly would lose a
                // passphrase the user watched themselves type.
                None if !typed_passphrase.is_empty() => {
                    failures.push(t(L10nKey::SettingsPassphraseNeedsKey).to_string())
                }
                None => {}
            }
        }

        for line in failures {
            window.push_notification(line, cx);
        }

        // What the form would now read back, so a save leaves it clean.
        if let Some(form) = self.ssh_form_mut() {
            form.loaded_password = typed;
            form.loaded_passphrase = typed_passphrase;
            form.loaded_endpoint = (profile.user.clone(), profile.host.clone(), profile.port);
            form.loaded_key = key;
        }
    }

    /// A blank secret deletes rather than stores: an empty string is not a
    /// passphrase, and leaving one behind would keep offering it.
    fn write_key_passphrase(&self, key_path: &str, secret: &str, failures: &mut Vec<String>) {
        let path = crate::core::ssh_profile::expand_tilde(key_path);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) => {
                failures.push(t_fmt(
                    L10nKey::SettingsCouldntSavePassphrase,
                    &[("key", key_path), ("error", &e.to_string())],
                ));
                return;
            }
        };
        let account = key_account_from_contents(&bytes);
        let result = if secret.is_empty() {
            OsCredentialStore.delete_key_passphrase(&account)
        } else {
            OsCredentialStore
                .set_key_passphrase(&account, secret)
                .map(|_| ())
        };
        if let Err(e) = result {
            failures.push(t_fmt(
                L10nKey::SettingsCouldntSavePassphrase,
                &[("key", key_path), ("error", &e.to_string())],
            ));
        }
    }

    /// Whether some other saved host still dials this endpoint. The keychain
    /// entry is the address's, not the profile's — the same reason "Forget
    /// password" counts the hosts it would sign out.
    fn endpoint_still_in_use(
        &self,
        user: &str,
        host: &str,
        port: u16,
        except: Uuid,
        cx: &App,
    ) -> bool {
        cx.global::<Config>()
            .ssh_profiles
            .iter()
            .any(|p| p.id != except && p.user == user && p.host == host && p.port == port)
    }

    /// Whether the SSH profile form on screen holds edits that were never
    /// saved. Save is enabled off exactly this, so closing on it is the same
    /// question the button already answers — and it compares what the form
    /// would save even when the form cannot be saved yet, so a half-typed new
    /// host is still something Escape has to ask about.
    pub(crate) fn ssh_form_dirty(&self, cx: &App) -> bool {
        let Some(form) = self.active_settings().and_then(|s| s.ssh_form.as_ref()) else {
            return false;
        };
        let saved = cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .find(|p| p.id == form.editing)
            .cloned();
        self.ssh_form_collect(cx).map(|(profile, _)| profile) != saved || form.secrets_changed(cx)
    }

    /// Closing from Escape or the X is the user leaving; every other caller
    /// closes as the tail of something they explicitly chose, and has already
    /// saved or does not care.
    pub(crate) fn close_settings_checked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.with_settings_edits_resolved(window, cx, |this, window, cx| {
            this.close_settings(window, cx)
        });
    }

    pub(crate) fn add_new_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profile = SshProfile::new(String::new());
        self.ssh_form_load(&profile, window, cx);
    }

    fn delete_profile_confirmed(&mut self, id: Uuid, cx: &mut Context<Self>) {
        // "Forget password" lives on the menu that is about to stop existing,
        // so deleting the profile used to strand its keychain entry with no UI
        // left to remove it. Only let go of the secret when nothing else on the
        // list still points at the same endpoint.
        let cfg = cx.global::<Config>();
        let endpoint = cfg
            .ssh_profiles
            .iter()
            .find(|p| p.id == id)
            .map(|p| (p.user.clone(), p.host.clone(), p.port));
        let shared = profiles_sharing_endpoint(cfg, id) > 0;
        if let Some((user, host, port)) = endpoint.filter(|_| !shared) {
            use crate::core::keychain::{CredentialStore, OsCredentialStore};
            let _ = OsCredentialStore.delete_password(&user, &host, port);
        }
        // The same argument for the key passphrases this profile taught the
        // app about: the comment above says "the secret", but until now only
        // the password was let go of, so a deleted profile stranded its
        // passphrase entries with no UI left to reach them. A key is only
        // forgotten when no surviving profile still lists it.
        use crate::core::keychain::{CredentialStore as _, OsCredentialStore};
        let mine = cfg
            .ssh_profiles
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.expanded_identity_files())
            .unwrap_or_default();
        let kept: std::collections::HashSet<String> = cfg
            .ssh_profiles
            .iter()
            .filter(|p| p.id != id)
            .flat_map(|p| p.expanded_identity_files())
            .collect();
        for path in mine.iter().filter(|p| !kept.contains(*p)) {
            // Keyed by the key file's contents, so a key already gone from
            // disk cannot be looked up — and has no live passphrase to leak.
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let account = crate::core::keychain::key_account_from_contents(&bytes);
            let _ = OsCredentialStore.delete_key_passphrase(&account);
        }

        // Forget the entries that routed through this profile (#485) —
        // forgotten, not deleted: `forget_workspace` never sends
        // `WorkspaceRemove`, so the remote sessions keep running and a new
        // profile to the same machine rediscovers them. Recomputed here
        // rather than carried from the prompt: the set can only have shrunk
        // (a new live link) while the dialog was up.
        let cascade = crate::ui::windows::cascade_for_profile(cx, id);
        for workspace in cascade {
            crate::ui::windows::forget_workspace(cx, workspace);
        }
        self.update_config(cx, |cfg| {
            cfg.ssh_profiles.retain(|p| p.id != id);
            cfg.ssh_profile_frecency.remove(&id);
        });
        if let Some(s) = self.active_settings_mut() {
            if s.ssh_form.as_ref().is_some_and(|f| f.editing == id) {
                s.ssh_form = None;
            }
            if s.ssh_open == Some(id) {
                s.ssh_open = None;
            }
            s.ssh_confirm_remove = false;
        }
        cx.notify();
    }

    /// Import `~/.ssh/config`, and say what that did.
    ///
    /// Every branch here ends in a notification because every branch used to
    /// end in nothing: a missing file, a file of nothing but `Host *`, and a
    /// clean import of six hosts were all the same silent button press, and the
    /// only way to tell them apart was to go count the host list.
    pub(crate) fn import_ssh_config_profiles(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // One id for all three outcomes, so pressing the button again replaces
        // what it said last time instead of stacking a second toast on top of
        // an answer that is now out of date.
        const NOTIFICATION: &str = "ssh-config-import";
        // A toast is 448pt wide. A config with a dozen unsupported keywords in
        // it would push the counts out of view, so the notification names the
        // first few and the log line below carries the whole list, with the
        // hosts each keyword was set on.
        const OPTIONS_SHOWN: usize = 5;

        let report = crate::core::ssh_config::import_report();
        let source = report.source.display().to_string();

        if !report.source_read {
            window.push_notification(
                Notification::error(t_fmt(
                    L10nKey::SettingsImportUnreadable,
                    &[("path", &source)],
                ))
                .id1::<Self>(NOTIFICATION),
                cx,
            );
            return;
        }
        if report.profiles.is_empty() {
            window.push_notification(
                Notification::warning(t_fmt(L10nKey::SettingsImportNoHosts, &[("path", &source)]))
                    .id1::<Self>(NOTIFICATION),
                cx,
            );
            return;
        }

        let read = report.profiles.len();
        let ignored = report.ignored;
        let mut stats = crate::core::ssh_config::MergeStats::default();
        self.update_config(cx, |cfg| {
            stats = crate::core::ssh_config::merge_imported(&mut cfg.ssh_profiles, report.profiles);
        });

        let dropped: Vec<String> = ignored
            .iter()
            .map(|opt| format!("{} ({})", opt.option, opt.hosts.join(", ")))
            .collect();
        log::info!(
            "imported {read} alias(es) from {source} ({} file(s) read): {} added, {} updated, \
             {} unchanged; no tty7 setting for: [{}]",
            report.files_read,
            stats.added,
            stats.updated,
            stats.unchanged,
            dropped.join("; ")
        );

        let mut notification = Notification::new()
            .with_type(NotificationType::Success)
            .title(t_plural(
                L10nKey::SettingsImportSummary,
                stats.added,
                &[
                    ("updated", &stats.updated.to_string()),
                    ("unchanged", &stats.unchanged.to_string()),
                ],
            ))
            .id1::<Self>(NOTIFICATION);
        if !ignored.is_empty() {
            let mut options: Vec<String> = ignored
                .iter()
                .take(OPTIONS_SHOWN)
                .map(|opt| opt.option.clone())
                .collect();
            let rest = ignored.len() - options.len();
            if rest > 0 {
                options.push(t_fmt(
                    L10nKey::SettingsImportMoreOptions,
                    &[("count", &rest.to_string())],
                ));
            }
            notification = notification
                .message(t_plural(
                    L10nKey::SettingsImportIgnored,
                    ignored.len(),
                    &[("options", &options.join(", "))],
                ))
                // A list of what the import could not carry is something to
                // read and act on, and four seconds is not long enough to do
                // either. The counts alone still fade on their own.
                .autohide(false);
        }
        window.push_notification(notification, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_has_unique_titles_and_default_values_are_unmodified() {
        let defaults = Config::default();
        for (i, entry) in settings_search_entries().iter().enumerate() {
            assert!(
                !settings_search_entries()[..i]
                    .iter()
                    .any(|e| e.title == entry.title),
                "duplicate {:?}",
                entry.title
            );
            assert!(
                !entry.modified(&defaults),
                "default {:?} is modified",
                entry.title
            );
            assert!(SettingsSection::ALL.contains(&entry.section));
        }
        assert_eq!(SettingsSection::ALL.len(), 8);
        assert!(!SettingsSection::ALL.contains(&SettingsSection::Keybindings));
    }

    #[test]
    fn config_keys_and_cross_language_names_reach_the_same_setting() {
        for locale in ["en", "zh-CN", "ja-JP"] {
            crate::ui::i18n::set_locale(locale);
            for (query, title, section) in [
                (
                    "gui_language",
                    L10nKey::SettingsLanguage,
                    SettingsSection::General,
                ),
                (
                    "mouse_zoom_modifier",
                    L10nKey::SettingsMouseZoom,
                    SettingsSection::KeyboardMouse,
                ),
                (
                    "per_pane_history",
                    L10nKey::SettingsPerPaneHistory,
                    SettingsSection::Terminal,
                ),
                (
                    "ui_font_size",
                    L10nKey::SettingsUiFontSize,
                    SettingsSection::Appearance,
                ),
                (
                    "Shell program",
                    L10nKey::SettingsProgram,
                    SettingsSection::Terminal,
                ),
            ] {
                let entry = settings_search_entries()
                    .iter()
                    .find(|e| e.title == title)
                    .unwrap();
                assert!(entry_matches(entry, query), "{locale}: {query}");
                assert_eq!(
                    best_matching_section(query).unwrap().profile_label(),
                    section.profile_label()
                );
            }
        }
        crate::ui::i18n::set_locale("en");
    }

    #[test]
    fn settings_row_ids_survive_language_changes() {
        for key in [
            L10nKey::SettingsFontSize,
            L10nKey::SettingsLanguage,
            L10nKey::SettingsMouseZoom,
        ] {
            crate::ui::i18n::set_locale("en");
            let expected = settings_row_id(t(key), "");
            for locale in ["zh-CN", "ja-JP"] {
                crate::ui::i18n::set_locale(locale);
                assert_eq!(settings_row_id(t(key), ""), expected);
            }
        }
        crate::ui::i18n::set_locale("en");
    }

    #[test]
    fn modified_settings_compare_their_own_values_only() {
        let mut cfg = Config::default();
        cfg.notify_threshold_secs += 1;
        let changed = settings_search_entries()
            .iter()
            .filter(|e| e.modified(&cfg))
            .map(|e| e.title)
            .collect::<Vec<_>>();
        assert_eq!(changed, vec![L10nKey::SettingsNotifyThreshold]);
    }

    /// A shortcut is the first thing someone searching a settings window for a
    /// feature by name is after, and the Keybindings page was the one page the
    /// search could not see into: searching "split" found the settings that
    /// merely mention splits and never the row labelled exactly that (#444).
    #[test]
    fn searching_for_a_feature_finds_its_shortcut() {
        crate::ui::i18n::set_locale("en");
        let kb = SettingsSection::Keybindings;

        // Split Right and Split Down, at least — the index carries no entry
        // for either, so before this every one of these counts was zero.
        assert!(
            section_match_count(kb, "split") >= 2,
            "got {}",
            section_match_count(kb, "split")
        );
        assert!(keybinding_matches_query("SplitRight", "split right"));

        // The action name is what the docs and `keybindings.json` spell, so a
        // reader arriving from either finds the row they read about.
        assert!(keybinding_matches_query("ScmSync", "scmsync"));

        // An empty query matches no row, or clearing the box would filter the
        // page down to nothing rather than back to every row. (The count above
        // it answers `contains("")` for the one indexed entry this section has
        // always carried, which is why the page gates on the query itself
        // before it consults either.)
        assert!(!keybinding_matches_query("SplitRight", ""));
        assert_eq!(keybinding_match_count(""), 0);

        // A query this page cannot answer leaves it alone — the page only
        // filters itself when it has something to show.
        assert_eq!(section_match_count(kb, "no such action anywhere"), 0);
    }

    /// The password lives in the keychain under the address, not in the
    /// profile — so saving has to decide two things the config file cannot
    /// record: whether the entry the form read is now stranded, and whether
    /// there is anything new to write.
    #[test]
    fn saving_moves_a_password_with_the_address_it_belongs_to() {
        let plan = |was, typed, moved| password_plan(was, typed, moved);

        // A form nobody typed in writes nothing at all.
        assert_eq!(
            plan("hunter2", "hunter2", false),
            PasswordPlan {
                drop_old: false,
                store: false
            }
        );
        // A new secret replaces the old one in place.
        assert_eq!(
            plan("hunter2", "correct horse", false),
            PasswordPlan {
                drop_old: false,
                store: true
            }
        );
        // Clearing the box is how a saved password is let go of.
        assert_eq!(
            plan("hunter2", "", false),
            PasswordPlan {
                drop_old: true,
                store: false
            }
        );
        // Retargeting the host carries the secret across and leaves nothing
        // behind under the old address — even when the secret itself is
        // untouched, because the account it is filed under is the address.
        assert_eq!(
            plan("hunter2", "hunter2", true),
            PasswordPlan {
                drop_old: true,
                store: true
            }
        );
        // A host that never had one, and still does not.
        assert_eq!(
            plan("", "", true),
            PasswordPlan {
                drop_old: false,
                store: false
            }
        );
        // The first password a host is given.
        assert_eq!(
            plan("", "hunter2", false),
            PasswordPlan {
                drop_old: false,
                store: true
            }
        );
    }

    /// A key picked from the system dialog arrives as an absolute path under
    /// the home directory. Written back that way it names the right file on
    /// this machine and the wrong one everywhere else — and `~` is how the
    /// rest of the field, and `~/.ssh/config` itself, spells it.
    #[test]
    fn a_picked_key_is_written_the_way_the_config_spells_it() {
        let home = Some("/Users/ada");
        assert_eq!(
            tildify_with("/Users/ada/.ssh/id_ed25519", home),
            "~/.ssh/id_ed25519"
        );
        // Outside the home directory there is nothing to shorten.
        assert_eq!(tildify_with("/etc/ssh/key", home), "/etc/ssh/key");
        // And a sibling that merely starts with the same letters is not
        // inside it.
        assert_eq!(
            tildify_with("/Users/adalovelace/key", home),
            "/Users/adalovelace/key"
        );
        // A trailing separator on the home directory changes nothing.
        assert_eq!(
            tildify_with("/Users/ada/.ssh/id_rsa", Some("/Users/ada/")),
            "~/.ssh/id_rsa"
        );
        // Nowhere to anchor against leaves the path as it came.
        assert_eq!(
            tildify_with("/Users/ada/.ssh/id_rsa", None),
            "/Users/ada/.ssh/id_rsa"
        );
    }

    /// The passphrase box is about one key at a time: the first one named that
    /// is actually on disk, with `%h` and `%r` filled in the way the daemon
    /// will fill them. A path that is not there can hold no passphrase.
    #[test]
    fn the_passphrase_follows_the_first_key_that_is_really_there() {
        let dir = std::env::temp_dir().join(format!("tty7-keyform-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("id_example.com");
        std::fs::write(&real, b"key").unwrap();
        let missing = dir.join("absent").to_string_lossy().to_string();
        let pattern = dir.join("id_%h").to_string_lossy().to_string();

        assert_eq!(
            first_readable_key_in(&[missing.clone(), pattern], "example.com", "ada"),
            Some(real.to_string_lossy().to_string())
        );
        assert_eq!(
            first_readable_key_in(&[missing.clone()], "example.com", "ada"),
            None
        );
        // An empty field falls back to the defaults the handshake offers —
        // and a named key, even a missing one, replaces them entirely.
        let defaults = || vec![missing.clone(), real.to_string_lossy().to_string()];
        assert_eq!(
            first_readable_key_or(&[], "example.com", "ada", defaults),
            Some(real.to_string_lossy().to_string())
        );
        assert_eq!(
            first_readable_key_or(&[missing.clone()], "example.com", "ada", defaults),
            None
        );
        assert_eq!(
            first_readable_key_or(&[], "example.com", "ada", Vec::new),
            None
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_row_stacks_once_its_label_and_control_stop_fitting() {
        assert!(settings_row_width(1440., 1.) >= STACK_ROW_BELOW);
        assert!(settings_row_width(900., 1.) >= STACK_ROW_BELOW);
        // At the narrowest window that turns up the nav gives back what it can
        // and the page is still under the width a label and a control share.
        assert_eq!(nav_width(640., 1.), NAV_W_MIN);
        assert!(settings_row_width(640., 1.) < STACK_ROW_BELOW);
        // Capped at the reading column, so a wider window never widens the row.
        assert_eq!(settings_row_width(4000., 1.), READING_COLUMN);
        // And never goes negative on a window narrower than its own chrome.
        assert_eq!(settings_row_width(100., 1.), 0.);
    }

    /// A preset row lights up the bucket the value *is*, and nothing when it
    /// is none of them — the range match it used to do labelled a hand-set
    /// value with a number the config did not hold, and the row carried no
    /// digits anywhere to correct it (#550).
    #[test]
    fn a_preset_row_highlights_only_the_bucket_the_value_actually_is() {
        // The default lands on a bucket, so the common case still reads as a
        // plain radio row.
        let (sel, custom) = preset_choice(
            &SCROLLBACK_BUCKETS,
            Config::default().scrollback_limit,
            group_thousands,
        );
        assert_eq!((sel, custom), (Some(1), None));

        // 50,000 is the value `docs/reference/configuration.mdx` puts in its
        // example config, so this is what following the documentation shows.
        let (sel, custom) = preset_choice(&SCROLLBACK_BUCKETS, 50_000, group_thousands);
        assert_eq!(sel, None, "50,000 is not one of the presets");
        let custom = custom.expect("a value off the presets names itself");
        assert!(
            custom.contains("50,000"),
            "the custom cell has to carry the real value, got {custom:?}"
        );

        // Boundaries: the old range match lit "10,000" for everything from
        // 1,001 up, and "100,000" for everything above that.
        assert_eq!(
            preset_choice(&SCROLLBACK_BUCKETS, 1_001, group_thousands).0,
            None
        );
        assert_eq!(
            preset_choice(&SCROLLBACK_BUCKETS, 100_000, group_thousands).0,
            Some(2)
        );

        // Same rule on the notify row, where 20s used to light up "30s".
        let (sel, custom) = preset_choice(&NOTIFY_THRESHOLD_BUCKETS, 20, |secs| format!("{secs}s"));
        assert_eq!(sel, None);
        assert!(custom.is_some_and(|c| c.contains("20s")));
        assert_eq!(
            preset_choice(&NOTIFY_THRESHOLD_BUCKETS, 60, |secs| format!("{secs}s")).0,
            Some(3),
            "60s is the '1m' cell, not a custom value"
        );
    }

    /// Each preset cell has to name the number clicking it writes, and the
    /// custom cell has to be written the same way as the cells beside it.
    #[test]
    fn preset_row_labels_name_the_value_they_write() {
        assert_eq!(SCROLLBACK_BUCKETS.len(), SCROLLBACK_LABELS.len());
        for (bucket, label) in SCROLLBACK_BUCKETS.iter().zip(SCROLLBACK_LABELS) {
            assert_eq!(group_thousands(*bucket), label);
        }
        assert_eq!(
            NOTIFY_THRESHOLD_BUCKETS.len(),
            NOTIFY_THRESHOLD_LABELS.len()
        );
        // Grouping starts at four digits and repeats every three.
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000_000), "1,000,000");
    }

    /// The thresholds are widths a *label* needs, and a reader who scaled the
    /// interface up scaled every label with it while the slider beside it kept
    /// the px width it was built at. A window that reads fine at the default
    /// font is a starved label column at the largest one.
    #[test]
    fn the_stacking_width_follows_the_interface_font() {
        use crate::core::config::UI_FONT_SIZE_MAX;
        let large = UI_FONT_SIZE_MAX / UI_FONT_SIZE_DEFAULT;
        // Side by side at the default font...
        assert!(settings_row_width(900., 1.) >= STACK_ROW_BELOW);
        // ...and stacked at the largest, where the same row holds half as much.
        assert!(
            settings_row_width(900., large) < STACK_ROW_BELOW * large,
            "a 900pt window at the largest interface font has to stack"
        );
        // The nav keeps its labels readable at the larger font.
        assert!(nav_width(1440., large) > NAV_W);
        // The reading column grows with the font, so a wide window does not.
        assert!(settings_row_width(1600., large) >= STACK_ROW_BELOW * large);
    }

    #[test]
    fn a_row_is_marked_by_its_label_or_by_the_keywords_behind_it() {
        // Straight label hit.
        assert!(row_matches_query(
            SettingsSection::Appearance,
            "Blur",
            "blur"
        ));
        // Keyword hit: the label says "Theme" and nothing more, but the index
        // says that row answers "palette".
        assert!(row_matches_query(
            SettingsSection::Appearance,
            t(L10nKey::SettingsThemeIntroTitle),
            "palette"
        ));
        // A row on some other page is not a hit just because the query matches
        // an entry elsewhere.
        assert!(!row_matches_query(
            SettingsSection::Terminal,
            "Blur",
            "palette"
        ));
        // An empty query marks nothing at all, so no page ever renders greyed
        // out just because the field is focused.
        assert!(!row_matches_query(SettingsSection::Appearance, "Blur", ""));
    }

    #[test]
    fn a_query_that_matches_nothing_is_distinguishable_from_one_that_does() {
        assert_eq!(total_match_count("zzqqxx"), 0);
        assert!(total_match_count("blur") > 0);
        assert!(total_match_count("palette") > 0);
    }

    #[test]
    fn settings_row_identity_depends_only_on_its_stable_label() {
        assert_eq!(
            settings_row_id("Claude Code", "Installingâ€¦"),
            settings_row_id("Claude Code", "Installed in C:\\tools")
        );
        assert_ne!(
            settings_row_id("Claude Code", "Installed"),
            settings_row_id("Codex", "Installed")
        );
    }

    #[test]
    fn synced_windows_backdrop_is_only_a_local_override_on_windows() {
        let mut config = Config::default();
        config.window_backdrop = WindowBackdrop::MicaAlt;

        assert!(window_overrides_active(&config, true));
        assert!(!window_overrides_active(&config, false));
    }

    #[test]
    fn opacity_and_blur_are_local_overrides_on_every_platform() {
        let mut opacity = Config::default();
        opacity.window_opacity = Some(0.8);
        opacity.window_backdrop = WindowBackdrop::Mica;
        let mut blur = Config::default();
        blur.window_blur = Some(true);
        blur.window_backdrop = WindowBackdrop::Acrylic;

        assert!(window_overrides_active(&opacity, false));
        assert!(window_overrides_active(&blur, false));
    }

    #[test]
    fn every_section_has_search_entries() {
        for section in SettingsSection::ALL {
            let n = settings_search_entries()
                .iter()
                .filter(|e| e.section == section)
                .count();
            assert!(
                n > 0,
                "section {:?} has no search entries",
                section.profile_label()
            );
        }
    }

    #[test]
    fn best_matching_section_can_reach_every_section() {
        for section in SettingsSection::ALL {
            let entry = settings_search_entries()
                .iter()
                .find(|e| e.section == section)
                .expect("checked by every_section_has_search_entries");
            let query = t(entry.title).to_lowercase();
            let landed = best_matching_section(&query);
            assert!(
                landed.is_some(),
                "query {query:?} matched nothing at all (section {:?})",
                section.profile_label()
            );
        }
    }

    #[test]
    fn previously_unsearchable_settings_are_findable() {
        use SettingsSection::*;
        let mut cases: Vec<(&str, SettingsSection)> = vec![
            ("opacity", Appearance),
            ("blur", Appearance),
            ("completion", Terminal),
            ("ctrl-r", Terminal),
            ("grouping", WindowTabs),
            ("threshold", General),
            ("report mouse", KeyboardMouse),
            ("nushell", Terminal),
            ("open files with", Terminal),
            ("bell", Terminal),
            ("known_hosts", Ssh),
            ("claude", Agents),
            ("symlink", Agents),
            // Rows the index had no entry for at all, so the query counted
            // nothing, no badge appeared and no row lit up: the whole Updates
            // group, and Smooth scrolling between two rows that were both
            // findable.
            ("smooth", Terminal),
            ("nightly", About),
            ("channel", About),
            ("metered", About),
            ("automatic", About),
            // A headline feature the index had never heard of: "background
            // image" matched nothing, and typing it walked the page to About
            // because "background" alone hits Download updates in the
            // background.
            ("background image", Appearance),
            ("wallpaper", Appearance),
            ("image opacity", Appearance),
        ];
        #[cfg(target_os = "windows")]
        cases.extend([
            ("material", Appearance),
            ("mica", Appearance),
            ("acrylic", Appearance),
        ]);
        for (query, expected) in cases {
            assert_eq!(
                best_matching_section(query).map(|s| s.profile_label()),
                Some(expected.profile_label()),
                "query {query:?} should land on {:?}",
                expected.profile_label()
            );
        }
    }

    /// The `tty7` CLI exists so scripts and coding agents can drive tty7, so it
    /// lives with the other agent integrations rather than under About.
    #[test]
    fn command_line_tool_is_searchable_under_agents() {
        let entry = settings_search_entries()
            .iter()
            .find(|entry| entry.title == L10nKey::SettingsInstallCliOnPath)
            .expect("the CLI setting should be searchable");

        assert_eq!(entry.section.profile_label(), "settings:agents");
    }

    #[test]
    fn index_titles_match_rendered_row_labels() {
        for title in [
            "Starting directory",
            "Restore last layout",
            "Terminal bell",
            "Report mouse to apps",
            "Open files with",
            "Sidebar grouping",
            "Tab completion",
            "Command history search",
            "Dim inactive panes",
            "Option (⌥) acts as Meta",
            "Install the tty7 command on PATH",
        ] {
            if title == "Option (⌥) acts as Meta" && !cfg!(target_os = "macos") {
                continue;
            }
            assert!(
                settings_search_entries()
                    .iter()
                    .any(|e| t(e.title) == title),
                "no index entry titled {title:?}"
            );
        }
    }

    #[test]
    fn agent_rows_are_in_the_search_index() {
        for agent in crate::core::agent_hooks::HookAgent::ALL {
            assert!(
                settings_search_entries()
                    .iter()
                    .any(|e| e.section == SettingsSection::Agents
                        && t(e.title) == agent.display_name()),
                "no Agents index entry titled {:?}",
                agent.display_name()
            );
        }
    }

    #[test]
    fn humanize_action_splits_on_capitals() {
        assert_eq!(humanize_action("NewTab"), "New Tab");
        assert_eq!(
            humanize_action("ToggleMaximizePane"),
            "Toggle Maximize Pane"
        );
        assert_eq!(humanize_action("Quit"), "Quit");
    }

    #[test]
    fn the_host_filter_matches_name_address_and_port() {
        let mut p = SshProfile::new("prod-web");
        p.host = "10.0.1.21".to_string();
        p.user = "deploy".to_string();
        p.port = 2222;

        assert!(ssh_row_matches(&p, ""), "an empty query keeps everything");
        assert!(ssh_row_matches(&p, "prod"));
        assert!(ssh_row_matches(&p, "10.0.1"));
        assert!(ssh_row_matches(&p, "deploy"));
        assert!(ssh_row_matches(&p, "2222"));
        assert!(!ssh_row_matches(&p, "staging"));
    }

    #[test]
    fn the_host_filter_ignores_case() {
        let mut p = SshProfile::new("Prod-Web");
        p.host = "Example.COM".to_string();
        assert!(ssh_row_matches(&p, "prod"));
        assert!(ssh_row_matches(&p, "example.com"));
    }

    #[test]
    fn group_buckets_sort_imported_first_and_ungrouped_last() {
        let mut keys = vec!["", "Work", crate::core::ssh_config::IMPORTED_GROUP];
        keys.sort_by_key(|k| ssh_group_rank(k));
        assert_eq!(
            keys,
            vec![crate::core::ssh_config::IMPORTED_GROUP, "Work", ""]
        );
    }

    #[test]
    fn group_labels_name_the_file_and_the_app() {
        assert_eq!(
            ssh_group_label(crate::core::ssh_config::IMPORTED_GROUP),
            "~/.ssh/config"
        );
        assert_eq!(ssh_group_label(""), "In tty7");
        assert_eq!(ssh_group_label("Work"), "Work");
    }

    #[test]
    fn group_key_falls_back_to_the_ungrouped_bucket() {
        let mut p = SshProfile::new("a");
        assert_eq!(ssh_group_key(&p), "");
        p.group = Some("Work".to_string());
        assert_eq!(ssh_group_key(&p), "Work");
    }

    #[test]
    fn parse_host_port_handles_blank_and_ports() {
        assert!(
            parse_host_port_checked("  ", DEFAULT_SOCKS_PORT)
                .unwrap()
                .is_none()
        );
        let hp = parse_host_port_checked("example.com:2222", DEFAULT_SOCKS_PORT)
            .unwrap()
            .unwrap();
        assert_eq!(hp.host, "example.com");
        assert_eq!(hp.port, 2222);
        // Used to be port 0, which no proxy answers on.
        assert_eq!(
            parse_host_port_checked("host", DEFAULT_SOCKS_PORT)
                .unwrap()
                .unwrap()
                .port,
            DEFAULT_SOCKS_PORT
        );
    }

    /// A form with the one field that is genuinely required, and nothing else.
    fn draft_with_host() -> SshFormDraft {
        SshFormDraft {
            host: "example.com".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn a_profile_with_no_host_is_not_saveable() {
        let (_, errors) = validate_ssh_draft(SshFormDraft::default(), &[]);
        assert_eq!(errors.host, Some(SshFieldError::HostMissing));
        assert!(!errors.is_empty());
    }

    #[test]
    fn spaces_are_not_a_host() {
        let draft = SshFormDraft {
            host: "   ".to_string(),
            ..Default::default()
        };
        let (profile, errors) = validate_ssh_draft(draft, &[]);
        assert_eq!(errors.host, Some(SshFieldError::HostMissing));
        assert_eq!(profile.host, "");
    }

    #[test]
    fn a_name_is_not_required() {
        // Every host imported from ~/.ssh/config arrives without one, and the
        // list falls back to the address.
        let (profile, errors) = validate_ssh_draft(draft_with_host(), &[]);
        assert_eq!(profile.name, "");
        assert!(errors.is_empty());
    }

    #[test]
    fn a_blank_port_still_means_22() {
        let (profile, errors) = validate_ssh_draft(draft_with_host(), &[]);
        assert_eq!(profile.port, 22);
        assert_eq!(errors.port, None);
    }

    #[test]
    fn a_port_that_is_not_a_port_is_refused() {
        // "0" parses as a u16 and used to be saved as written; the other two
        // failed to parse and were silently rewritten to 22.
        for text in ["0", "abc", "70000", "-1", "22 "] {
            let draft = SshFormDraft {
                port: text.to_string(),
                ..draft_with_host()
            };
            let (profile, errors) = validate_ssh_draft(draft, &[]);
            match text {
                "22 " => {
                    assert_eq!(errors.port, None, "{text:?} is a port with spare space");
                    assert_eq!(profile.port, 22);
                }
                _ => {
                    assert_eq!(errors.port, Some(SshFieldError::PortRange), "{text:?}");
                    assert!(!errors.is_empty());
                }
            }
        }
    }

    #[test]
    fn a_jump_host_that_exists_is_kept_by_id() {
        let bastion = SshProfile::new("bastion");
        let draft = SshFormDraft {
            jump: "bastion".to_string(),
            ..draft_with_host()
        };
        let (profile, errors) = validate_ssh_draft(draft, &[bastion.clone()]);
        assert_eq!(profile.jump_host, Some(bastion.id));
        assert!(errors.is_empty());
    }

    #[test]
    fn a_mistyped_jump_host_says_which_name_it_could_not_find() {
        let draft = SshFormDraft {
            jump: "bastian".to_string(),
            ..draft_with_host()
        };
        let (profile, errors) = validate_ssh_draft(draft, &[SshProfile::new("bastion")]);
        assert_eq!(
            errors.jump,
            Some(SshFieldError::JumpUnknown("bastian".to_string()))
        );
        assert_eq!(
            profile.jump_host, None,
            "a typo never saves as a direct connection"
        );
    }

    #[test]
    fn a_host_cannot_jump_through_itself() {
        let me = SshProfile::new("prod");
        let draft = SshFormDraft {
            id: me.id,
            jump: "prod".to_string(),
            ..draft_with_host()
        };
        let (profile, errors) = validate_ssh_draft(draft, &[me]);
        assert_eq!(errors.jump, Some(SshFieldError::JumpIsSelf));
        assert_eq!(profile.jump_host, None);
    }

    #[test]
    fn a_bare_proxy_host_takes_the_scheme_default_port() {
        let draft = SshFormDraft {
            socks: "socks.example.com".to_string(),
            http: "http.example.com".to_string(),
            ..draft_with_host()
        };
        let (profile, errors) = validate_ssh_draft(draft, &[]);
        assert_eq!(
            profile.socks_proxy,
            Some(HostPort::new("socks.example.com", 1080))
        );
        assert_eq!(
            profile.http_proxy,
            Some(HostPort::new("http.example.com", 8080))
        );
        assert!(errors.is_empty());
    }

    #[test]
    fn a_proxy_address_with_a_colon_and_no_port_is_refused() {
        for text in ["proxy.example.com:", "proxy.example.com:abc", "proxy:0"] {
            let draft = SshFormDraft {
                socks: text.to_string(),
                ..draft_with_host()
            };
            let (profile, errors) = validate_ssh_draft(draft, &[]);
            assert_eq!(
                errors.socks,
                Some(SshFieldError::ProxyPortRange),
                "{text:?}"
            );
            assert_eq!(profile.socks_proxy, None, "{text:?}");
        }
    }

    #[test]
    fn a_form_that_cannot_be_saved_still_reports_what_it_would_save() {
        // The Escape prompt asks whether the form differs from the config, so
        // an invalid form has to hand back a profile to compare — otherwise a
        // half-typed new host looks identical to the nothing on disk and
        // Escape throws it away without asking.
        let draft = SshFormDraft {
            name: "half typed".to_string(),
            ..Default::default()
        };
        let (profile, errors) = validate_ssh_draft(draft, &[]);
        assert!(!errors.is_empty());
        assert_eq!(profile.name, "half typed");
    }

    fn profile_at(name: &str, user: &str, host: &str, port: u16) -> SshProfile {
        let mut p = SshProfile::new(name);
        p.user = user.to_string();
        p.host = host.to_string();
        p.port = port;
        p
    }

    /// The saved password belongs to `user@host:port`, so what counts as
    /// "shared" is exactly that triple — a different name or a jump host in
    /// front of it changes nothing, and a different port makes it a different
    /// secret entirely.
    #[test]
    fn the_same_endpoint_under_two_names_counts_as_shared() {
        let direct = profile_at("direct", "ana", "build.example.com", 22);
        let mut via_jump = profile_at("via bastion", "ana", "build.example.com", 22);
        via_jump.jump_host = Some(direct.id);
        let staging = profile_at("staging", "ana", "build.example.com", 2222);
        let other_user = profile_at("root", "root", "build.example.com", 22);

        let mut cfg = Config::default();
        let (direct_id, jump_id, staging_id) = (direct.id, via_jump.id, staging.id);
        cfg.ssh_profiles = vec![direct, via_jump, staging, other_user];

        // The two that reach the same endpoint see each other, and neither
        // counts itself.
        assert_eq!(profiles_sharing_endpoint(&cfg, direct_id), 1);
        assert_eq!(profiles_sharing_endpoint(&cfg, jump_id), 1);
        // A port apart is a keychain entry apart, so this one is alone even
        // though the user and host match two of the others.
        assert_eq!(profiles_sharing_endpoint(&cfg, staging_id), 0);
        // A profile that is no longer on the list shares with nobody.
        assert_eq!(profiles_sharing_endpoint(&cfg, Uuid::new_v4()), 0);
    }
}

#[cfg(test)]
mod gpui_tests {
    use super::SettingsSection;
    use crate::core::config::{Config, MouseZoomModifier};
    use crate::core::session::Session;
    use crate::ui::app::Tty7App;
    use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext, px, size};

    fn harness(cx: &mut TestAppContext) -> (Entity<Tty7App>, VisualTestContext) {
        crate::core::config::pin_test_config_dir();
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            crate::ui::keymap::init(cx);
        });
        let window = cx.add_window(|window, cx| {
            let app =
                cx.new(|cx| Tty7App::with_session(None, Some(Session::default()), window, cx));
            gpui_component::Root::new(app, window, cx)
        });
        cx.background_executor.run_until_parked();
        let app = window
            .update(cx, |root, _, _| {
                root.view()
                    .clone()
                    .downcast::<Tty7App>()
                    .unwrap_or_else(|_| panic!("window root wraps a Tty7App"))
            })
            .unwrap();
        let vcx = VisualTestContext::from_window(window.into(), cx);
        (app, vcx)
    }

    /// The password is the one field on the host editor that never reaches the
    /// profile — it goes to the system keychain — so the dirty check that
    /// compares profiles is blind to it. Without the secret folded in, Save
    /// stays greyed out over a password the user has just typed, and the only
    /// way to store one is to connect and wait to be asked.
    #[gpui::test]
    fn a_typed_password_is_something_the_form_has_to_save(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::Ssh, window, cx);
            // A saved host that names no address, so opening its editor asks
            // the keychain nothing and starts out with nothing to save.
            let profile = crate::core::ssh_profile::SshProfile::new("blank");
            app.update_config(cx, |cfg| cfg.ssh_profiles.push(profile.clone()));
            app.ssh_form_load(&profile, window, cx);
        });
        vcx.simulate_resize(size(px(1100.), px(800.)));
        vcx.run_until_parked();

        assert!(
            !vcx.update(|_, cx| app.read(cx).ssh_form_dirty(cx)),
            "a form nobody has typed in has nothing to save"
        );

        let password = vcx.update(|_, cx| {
            app.read(cx)
                .active_settings()
                .and_then(|s| s.ssh_form.as_ref())
                .map(|f| f.password.clone())
                .expect("the host editor is open")
        });
        app.update_in(&mut vcx, |_app, window, cx| {
            password.update(cx, |input, cx| input.set_value("hunter2", window, cx));
        });
        vcx.run_until_parked();

        assert!(
            vcx.update(|_, cx| app.read(cx).ssh_form_dirty(cx)),
            "a password typed into the form is an unsaved change"
        );
    }

    #[gpui::test]
    fn enter_on_a_shortcut_search_result_opens_its_local_filter(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::General, window, cx);
            let input = app.active_settings().unwrap().search.clone();
            input.update(cx, |input, cx| input.set_value("SplitRight", window, cx));
            app.autoselect_settings_search(cx);
        });
        vcx.run_until_parked();
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, _, cx| {
            let state = app.active_settings().unwrap();
            assert!(state.section == SettingsSection::Keybindings);
            assert!(!state.search_active);
            assert_eq!(
                state.shortcut_search.read(cx).value().as_str(),
                "SplitRight"
            );
        });
    }

    #[gpui::test]
    fn external_ssh_navigation_resolves_the_current_form_once(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);
        let mut original = crate::core::ssh_profile::SshProfile::new("original");
        original.host = "original.example.com".into();
        let id = original.id;
        app.update_in(&mut vcx, |app, window, cx| {
            cx.global_mut::<Config>()
                .ssh_profiles
                .push(original.clone());
            app.open_ssh_profile_in_settings(id, window, cx);
            let input = app
                .active_settings()
                .unwrap()
                .ssh_form
                .as_ref()
                .unwrap()
                .host
                .clone();
            input.update(cx, |input, cx| {
                input.set_value("edited.example.com", window, cx)
            });
            app.open_ssh_profile_new_from_target("new.example.com".into(), window, cx);
        });
        vcx.run_until_parked();
        assert!(vcx.has_pending_prompt());
        vcx.simulate_prompt_answer(crate::ui::i18n::t(
            crate::ui::i18n::L10nKey::SettingsKeepEditing,
        ));
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, window, cx| {
            assert_eq!(
                app.active_settings()
                    .unwrap()
                    .ssh_form
                    .as_ref()
                    .unwrap()
                    .editing,
                id
            );
            assert!(app.ssh_form_dirty(cx));
            app.open_ssh_profile_new_from_target("new.example.com".into(), window, cx);
        });
        vcx.run_until_parked();
        assert!(vcx.has_pending_prompt());
        vcx.simulate_prompt_answer(crate::ui::i18n::t(crate::ui::i18n::L10nKey::EditorDiscard));
        vcx.run_until_parked();
        assert!(!vcx.has_pending_prompt());
        app.update_in(&mut vcx, |app, _, cx| {
            let form = app.active_settings().unwrap().ssh_form.as_ref().unwrap();
            assert_ne!(form.editing, id);
            assert_eq!(form.host.read(cx).value().as_str(), "new.example.com");
        });
        app.update_in(&mut vcx, |app, window, cx| {
            app.cancel_ssh_form(cx);
            app.open_ssh_profile_in_settings(id, window, cx);
            let input = app
                .active_settings()
                .unwrap()
                .ssh_form
                .as_ref()
                .unwrap()
                .host
                .clone();
            input.update(cx, |s, cx| s.set_value("saved.example.com", window, cx));
            app.open_ssh_profile_in_settings(id, window, cx);
        });
        vcx.run_until_parked();
        vcx.simulate_prompt_answer(crate::ui::i18n::t(
            crate::ui::i18n::L10nKey::SettingsSaveChanges,
        ));
        vcx.run_until_parked();
        assert!(!vcx.has_pending_prompt());
        app.update_in(&mut vcx, |app, _, cx| {
            assert!(!app.ssh_form_dirty(cx));
            assert_eq!(
                app.active_settings()
                    .unwrap()
                    .ssh_form
                    .as_ref()
                    .unwrap()
                    .host
                    .read(cx)
                    .value()
                    .as_str(),
                "saved.example.com"
            );
        });
    }

    #[gpui::test]
    fn failed_settings_writes_remain_visible_until_retry_succeeds(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::General, window, cx);
            cx.global_mut::<Config>().quarantined = true;
            app.set_notify_threshold(73, cx);
            assert!(app.active_settings().unwrap().save_error.is_some());
            cx.global_mut::<Config>().quarantined = false;
            app.persist_settings_config(cx);
            assert!(app.active_settings().unwrap().save_error.is_none());
            assert_eq!(cx.global::<Config>().notify_threshold_secs, 73);
        });
        vcx.run_until_parked();
    }

    #[gpui::test]
    fn theme_edits_preview_without_writing_and_cancel_restores_original(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("draft.yaml");
        let (app, mut vcx) = harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            let mut theme = crate::ui::presets::all(cx).remove(0);
            theme.id = "settings-test-theme".into();
            theme.path = Some(path.clone());
            crate::ui::presets::write_theme_file(&theme).unwrap();
            let original_file = std::fs::read(&path).unwrap();
            let original_color = theme.accent;
            let mut themes = crate::ui::presets::all(cx);
            themes.push(theme);
            cx.set_global(crate::ui::presets::Themes(themes));
            cx.global_mut::<Config>().theme_follow_system = false;
            cx.global_mut::<Config>().theme_preset = "settings-test-theme".into();
            app.open_settings_section(SettingsSection::Appearance, window, cx);
            app.edit_active_theme(
                crate::ui::app::ThemeEdit::Accent,
                gpui::rgb(0x123456).into(),
                window,
                cx,
            );
            assert!(app.theme_draft_dirty());
            assert_eq!(std::fs::read(&path).unwrap(), original_file);
            app.cancel_theme_draft(window, cx);
            assert!(!app.theme_draft_dirty());
            assert_eq!(
                crate::ui::presets::by_id(cx, "settings-test-theme").accent,
                original_color
            );
            app.edit_active_theme(
                crate::ui::app::ThemeEdit::Accent,
                gpui::rgb(0x654321).into(),
                window,
                cx,
            );
            assert!(app.save_theme_draft(window, cx));
            assert_ne!(std::fs::read(&path).unwrap(), original_file);
            assert!(!app.theme_draft_dirty());
            app.edit_active_theme(
                crate::ui::app::ThemeEdit::Accent,
                gpui::rgb(0x102030).into(),
                window,
                cx,
            );
            app.active_settings_mut()
                .unwrap()
                .theme_draft
                .as_mut()
                .unwrap()
                .1
                .path = Some(dir.path().to_path_buf());
            assert!(!app.save_theme_draft(window, cx));
            assert!(app.theme_draft_dirty());
            assert!(app.active_settings().unwrap().theme_draft_error.is_some());
        });
    }

    #[gpui::test]
    fn search_reuses_rows_and_reset_changes_only_the_selected_setting(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::General, window, cx);
            app.set_notify_threshold(71, cx);
            app.set_copy_on_select(!Config::default().copy_on_select, cx);
            let search = app.active_settings().unwrap().search.clone();
            search.update(cx, |s, cx| s.set_value("notify_threshold_secs", window, cx));
            app.autoselect_settings_search(cx);
        });
        vcx.simulate_resize(size(px(720.), px(560.)));
        vcx.run_until_parked();
        app.update_in(&mut vcx, |app, window, cx| {
            assert!(app.active_settings().unwrap().search_active);
            assert!(
                app.active_settings()
                    .unwrap()
                    .search_rows
                    .borrow()
                    .is_none()
            );
            app.reset_settings_value(
                crate::ui::i18n::L10nKey::SettingsNotifyThreshold,
                window,
                cx,
            );
            assert_eq!(
                cx.global::<Config>().notify_threshold_secs,
                Config::default().notify_threshold_secs
            );
            assert_eq!(
                cx.global::<Config>().copy_on_select,
                !Config::default().copy_on_select
            );
        });
        vcx.run_until_parked();
    }

    #[gpui::test]
    fn every_category_and_full_search_can_layout_at_minimum_width(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);
        for section in SettingsSection::ALL {
            app.update_in(&mut vcx, |app, window, cx| {
                app.open_settings_section(section, window, cx)
            });
            vcx.simulate_resize(size(px(720.), px(560.)));
            vcx.run_until_parked();
        }
        app.update_in(&mut vcx, |app, _, cx| {
            app.active_settings_mut().unwrap().modified_only = true;
            app.autoselect_settings_search(cx);
        });
        vcx.run_until_parked();
    }

    #[gpui::test]
    fn clearing_search_preserves_the_category_and_target_navigation_clears_search(
        cx: &mut TestAppContext,
    ) {
        let (app, mut vcx) = harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::WindowTabs, window, cx);
            let search = app.active_settings().unwrap().search.clone();
            search.update(cx, |s, cx| s.set_value("mouse", window, cx));
            app.autoselect_settings_search(cx);
            assert!(app.active_settings().unwrap().section == SettingsSection::WindowTabs);
            search.update(cx, |s, cx| s.set_value("", window, cx));
            app.autoselect_settings_search(cx);
            assert!(!app.active_settings().unwrap().search_active);
            app.navigate_settings(
                SettingsSection::KeyboardMouse,
                Some(crate::ui::i18n::L10nKey::SettingsMouseZoom),
                window,
                cx,
            );
            assert!(app.active_settings().unwrap().section == SettingsSection::KeyboardMouse);
        });
        vcx.run_until_parked();
    }

    #[gpui::test]
    fn appearance_section_lays_out_with_its_rounded_controls(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::Appearance, window, cx);
        });

        vcx.simulate_resize(size(px(1100.), px(800.)));
        vcx.run_until_parked();

        app.update_in(&mut vcx, |app, _, cx| {
            // The theme menu open: its popover has to lay out beside the page.
            if let Some(s) = app.active_settings_mut() {
                s.menu = Some("theme-slot-dark".into());
            }
            cx.notify();
        });
        vcx.simulate_resize(size(px(720.), px(560.)));
        vcx.run_until_parked();

        let section = vcx.update(|_, cx| app.read(cx).active_settings().map(|s| s.section));
        assert!(
            matches!(section, Some(SettingsSection::Appearance)),
            "the page should still be on Appearance after two paint passes",
        );
    }

    /// #668: the Terminal page carries the control that moves the zoom off the
    /// platform modifier, so the page has to paint with it, and the pick has to
    /// reach the config the wheel reads.
    #[gpui::test]
    fn the_terminal_page_paints_the_zoom_modifier_row(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::Terminal, window, cx);
        });
        vcx.simulate_resize(size(px(1100.), px(800.)));
        vcx.run_until_parked();

        let modifier = vcx.update(|_, cx| cx.global::<Config>().mouse_zoom_modifier);
        assert_eq!(
            modifier,
            MouseZoomModifier::Platform,
            "the wheel still zooms out of the box"
        );

        app.update_in(&mut vcx, |app, _, cx| {
            app.set_mouse_zoom_modifier(MouseZoomModifier::None, cx)
        });
        vcx.run_until_parked();
        let modifier = vcx.update(|_, cx| cx.global::<Config>().mouse_zoom_modifier);
        assert_eq!(modifier, MouseZoomModifier::None, "and the pick sticks");
    }

    /// The Input page paints with the prompt editor off — that is the state
    /// where two of its rows are greyed out and their switches disabled — and
    /// the cascade only *disables* those two. It must not rewrite what they
    /// hold, or turning the editor back on would hand the user a completion
    /// menu they had switched off.
    #[gpui::test]
    fn the_prompt_editor_greys_its_dependants_without_rewriting_them(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        let (app, mut vcx) = harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::Terminal, window, cx);
            app.set_history_search(false, cx);
            app.set_prompt_editor(false, cx);
        });
        vcx.simulate_resize(size(px(1100.), px(800.)));
        vcx.run_until_parked();

        let (prompt_editor, tab_completion, history_search) = vcx.update(|_, cx| {
            let cfg = cx.global::<Config>();
            (cfg.prompt_editor, cfg.tab_completion, cfg.history_search)
        });
        assert!(!prompt_editor, "the switch stuck");
        assert!(
            tab_completion,
            "a greyed-out row keeps its value for when the editor comes back"
        );
        assert!(!history_search, "and one the user had turned off stays off");

        app.update_in(&mut vcx, |app, _, cx| app.set_prompt_editor(true, cx));
        vcx.run_until_parked();
        let (prompt_editor, tab_completion) = vcx.update(|_, cx| {
            let cfg = cx.global::<Config>();
            (cfg.prompt_editor, cfg.tab_completion)
        });
        assert!(prompt_editor);
        assert!(tab_completion, "the completion menu comes back with it");
    }
}

use gpui::{
    Animation, AnimationExt as _, AnyElement, App, Background, Context, Div, Entity, FontWeight,
    Image, ImageFormat, KeyDownEvent, MouseButton, SharedString, Stateful, Subscription, Window,
    div, img, prelude::*, px, relative, rgb,
};
use gpui_component::InteractiveElementExt as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::color_picker::{ColorPicker, ColorPickerState};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::link::Link;
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::notification::{Notification, NotificationType};
use gpui_component::select::{SearchableVec, Select, SelectState};
use gpui_component::sidebar::{Sidebar, SidebarCollapsible, SidebarMenu, SidebarMenuItem};
use gpui_component::slider::{Slider, SliderState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex,
    v_flex,
};
use std::cell::Cell;
use std::sync::Arc;

use uuid::Uuid;

use crate::core::config::{
    BellMode, Config, CursorStyle, NewTabPosition, NotifyMode, TabBarPosition,
    UI_FONT_SIZE_DEFAULT, UpdateChannel, WindowBackdrop,
};
use crate::core::keychain::CredentialRef;
use crate::core::ssh_profile::{
    Algorithms, AuthMode, ForwardKind, ForwardRule, HostPort, SshProfile, to_connect_string,
};
use crate::ui::app::{
    FONT_SIZE_STEP, LINE_HEIGHT_STEP, TILE_GLYPH_LINE, TILE_SIZE, TITLE_BAR_HEIGHT, ThemeEdit,
    Tty7App, UI_FONT_SIZE_STEP,
};
use crate::ui::host_ops::HostId;
use crate::ui::i18n::{L10nKey, t, t_fmt, t_plural};
use crate::ui::presets;
use crate::ui::rounding;
use crate::ui::rounding::RoundedCorners as _;

/// The settings nav, the SSH host list, the theme panel, and the padding each
/// page sets — the chrome a row has to share the window with.
const NAV_W: f32 = 220.;
const SSH_LIST_W: f32 = 280.;
const THEME_PANEL_W: f32 = 300.;
const SSH_DETAIL_PAD: f32 = 64.;
const PAGE_PAD: f32 = 80.;

/// The narrowest window these numbers have to hold for.
///
/// `ui::windows::MIN_SIZE` declares 720, but the settings window in the report
/// this file was fixed for measured 641pt: `window_min_size` governs dragging,
/// not the bounds a window opens with, so a remembered bound walked straight
/// under it. `ui::windows::at_least_min_size` now closes that path, and this
/// stays below it deliberately — a declared minimum is a claim about the code,
/// and this file would rather be laid out for the window that turns up.
const NARROWEST_WINDOW: f32 = 640.;

/// The narrowest each list is still itself: a nav item that still shows a label
/// beside its icon (40 of icon, gap and padding, then the longest label), a
/// host row that still shows a name, a theme card that is still a recognisable
/// picture of a theme.
///
/// The nav floor is sized for the longest nav label in *any* locale, not the
/// one the developer happens to be reading. `SidebarMenuItem` clips its label
/// rather than eliding it, so a floor that fits English cuts a glyph in half in
/// Chinese and Japanese: at 140 the zh-CN "窗口与标签页" lost the right half of
/// its last character. The widest is ja-JP "ウィンドウとタブ" — 8 full-width
/// kana beside the icon, which is 36 more than the 6-glyph Chinese label needs.
const NAV_W_MIN: f32 = 176.;
const SSH_LIST_W_MIN: f32 = 180.;
const THEME_PANEL_W_MIN: f32 = 240.;

/// The reading column every scrolled page caps itself at. Wider than this and
/// a description stops being a paragraph and becomes a line to scan across.
const READING_COLUMN: f32 = 640.;

/// What the page gets before any list does, and the floor it may be pushed to
/// when even that cannot be had — the numbers this file did not have.
///
/// Every list beside the page was a fixed width that never gave anything back,
/// so the page absorbed the entire shortfall. On a half-width window with the
/// theme panel open that ran all the way down: a Chinese description came out
/// one character per line, twenty-five lines tall, and the theme cards under it
/// were slivers clipped by the window edge.
///
/// `CONTENT_W` holds a stacked row's widest control — the 260px text fields —
/// with a description beside it that still reads as a paragraph. `CONTENT_MIN_W`
/// is not chosen at all: it is what the narrowest window in the wild leaves the
/// SSH page, the one that spends a second list, once both lists are standing on
/// their own floors. A control wider than it has to be able to shrink, which is
/// what `max_w_full` on the wrappers below is for.
const CONTENT_W: f32 = 420.;
const CONTENT_MIN_W: f32 = NARROWEST_WINDOW - NAV_W_MIN - SSH_LIST_W_MIN - SSH_DETAIL_PAD;

/// What the settings page is made of at a given window width.
#[derive(Clone, Copy, PartialEq, Debug)]
struct SettingsColumns {
    nav: f32,
    /// Zero off the SSH page.
    ssh_list: f32,
    /// The width to *draw* the theme panel at, whether it is taking a column or
    /// covering one — zero only when it is closed.
    theme_panel: f32,
    /// The panel no longer fits beside the page, so it lays itself over the
    /// page instead of taking width from it. The panel is a temporary layer
    /// over one choice; the page underneath is what the window is for.
    panel_overlays: bool,
}

/// Hand every list its full width, then take the shortfall back from all of
/// them at once — each in proportion to what it has to spare — until the page
/// between them reaches `CONTENT_W`. Once every list is standing on its own
/// floor the page takes whatever is left, which from `NARROWEST_WINDOW` up is
/// never less than `CONTENT_MIN_W`. The theme panel leaves the row altogether
/// rather than let it come to that.
///
/// Every width here is one this row can actually have, so the columns always
/// add up to the window. That is deliberate: the fix for a page squeezed to
/// nothing is not a `min_w` the row cannot honour — a floor a flex row cannot
/// meet does not push back, it overflows, and overflow here means content
/// painted off the edge of the window, which is the other half of this bug.
fn settings_columns(
    section: SettingsSection,
    theme_panel_open: bool,
    viewport: f32,
) -> SettingsColumns {
    let ssh = matches!(section, SettingsSection::Ssh);
    // The panel belongs to Appearance; a stale open flag on any other page is
    // not a column, the same way `render_settings` does not draw one.
    let theme_panel_open = theme_panel_open && matches!(section, SettingsSection::Appearance);
    let pad = if ssh { SSH_DETAIL_PAD } else { PAGE_PAD };
    let page = CONTENT_W + pad;

    // Even with the nav and the panel both at their floors there has to be a
    // readable page left between them. Below that width the panel stops being a
    // column — this is the one place the *floor* is the test, because the panel
    // leaving the row is what buys the page its preferred width back.
    let panel_overlays =
        theme_panel_open && viewport - NAV_W_MIN - THEME_PANEL_W_MIN - PAGE_PAD < CONTENT_MIN_W;
    let beside = theme_panel_open && !panel_overlays;

    let mut nav = NAV_W;
    let mut ssh_list = only_when(ssh, SSH_LIST_W);
    let mut theme_panel = only_when(beside, THEME_PANEL_W);
    let (nav_slack, list_slack, panel_slack) = (
        NAV_W - NAV_W_MIN,
        only_when(ssh, SSH_LIST_W - SSH_LIST_W_MIN),
        only_when(beside, THEME_PANEL_W - THEME_PANEL_W_MIN),
    );
    let slack = nav_slack + list_slack + panel_slack;
    let short = (nav + ssh_list + theme_panel + page - viewport).max(0.);
    if short > 0. && slack > 0. {
        let give = (short / slack).min(1.);
        nav -= nav_slack * give;
        ssh_list -= list_slack * give;
        theme_panel -= panel_slack * give;
    }
    if panel_overlays {
        // Covering the page, not replacing it: leave a strip of the page in
        // view so the panel reads as something laid on top and dismissible.
        theme_panel = THEME_PANEL_W
            .min(viewport - nav - CONTENT_MIN_W / 2.)
            .max(THEME_PANEL_W_MIN);
    }
    SettingsColumns {
        nav: nav.round(),
        ssh_list: ssh_list.round(),
        theme_panel: theme_panel.round(),
        panel_overlays,
    }
}

/// What a row on this page really has to lay out in.
///
/// The nav is always in front of it; the SSH page puts its host list there too,
/// the theme panel takes another slice of Appearance for as long as it is open
/// *and* still fits beside it, and only the scrolled pages cap the reading
/// column.
fn settings_row_width(
    section: SettingsSection,
    theme_panel_open: bool,
    viewport: f32,
    ui_scale: f32,
) -> f32 {
    let cols = settings_columns(section, theme_panel_open, viewport);
    let panel = only_when(!cols.panel_overlays, cols.theme_panel);
    match section {
        SettingsSection::Ssh => (viewport - cols.nav - cols.ssh_list - SSH_DETAIL_PAD).max(0.),
        _ => (viewport - cols.nav - panel - PAGE_PAD).clamp(0., READING_COLUMN * ui_scale),
    }
}

fn only_when(on: bool, w: f32) -> f32 {
    if on { w } else { 0. }
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

/// Width a settings row needs before its label and its control fit side by
/// side: a 260px control, the `gap_8` between them, and enough left for a
/// description to read as prose rather than as a column of words.
const STACK_ROW_BELOW: f32 = 500.;

/// Width a port-forwarding rule needs before its kind switch, its two host:port
/// pairs, its description and its remove button all fit on one line: about 580
/// with every field at its floor, rounded up. Past this the rule takes two
/// lines instead of running off the page.
const SPLIT_FORWARD_ROW_BELOW: f32 = 620.;

/// And the width below which even `bind → target` is more than one line holds:
/// two host fields at their narrow floor, two ports and the arrow come to about
/// 310, which is more than `CONTENT_MIN_W`. The SSH page reaches this on the
/// window the report came from, so the two ends take a line each.
const STACK_FORWARD_ENDS_BELOW: f32 = 340.;

fn settings_row_id(label: &str, _desc: &str) -> SharedString {
    SharedString::from(format!("settings-row-{label}"))
}

fn settings_header_id(title: &str) -> SharedString {
    SharedString::from(format!("settings-header-{title}"))
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
    Appearance,
    Terminal,
    Input,
    Ssh,
    Agents,
    WindowTabs,
    Keybindings,
    About,
}

impl SettingsSection {
    pub(crate) const ALL: [SettingsSection; 8] = [
        SettingsSection::Appearance,
        SettingsSection::Terminal,
        SettingsSection::Input,
        SettingsSection::Ssh,
        SettingsSection::Agents,
        SettingsSection::WindowTabs,
        SettingsSection::Keybindings,
        SettingsSection::About,
    ];

    fn profile_label(self) -> &'static str {
        match self {
            SettingsSection::Appearance => "settings:appearance",
            SettingsSection::Terminal => "settings:terminal",
            SettingsSection::Input => "settings:input",
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
            section: Terminal,
            title: SettingsFocusFollowsMouse,
            keywords: SettingsSearchFocusFollowsMouseKeywords,
        },
        SearchEntry {
            section: Terminal,
            title: SettingsHideMouseWhileTyping,
            keywords: SettingsSearchHideMouseWhileTypingKeywords,
        },
        SearchEntry {
            section: Terminal,
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
            section: Input,
            title: SettingsTabCompletion,
            keywords: SettingsSearchTabCompletionKeywords,
        },
        SearchEntry {
            section: Input,
            title: SettingsHistorySearch,
            keywords: SettingsSearchHistorySearchKeywords,
        },
        SearchEntry {
            section: Input,
            title: SettingsOptionAsMeta,
            keywords: SettingsSearchOptionAsMetaKeywords,
        },
        SearchEntry {
            section: Input,
            title: SettingsSmartSelection,
            keywords: SettingsSearchSmartSelectionKeywords,
        },
        SearchEntry {
            section: Input,
            title: SettingsCopyOnSelect,
            keywords: SettingsSearchCopyOnSelectKeywords,
        },
        SearchEntry {
            section: Input,
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
            section: Ssh,
            title: SettingsPortForwarding,
            keywords: SettingsSearchPortForwardingKeywords,
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
            section: WindowTabs,
            title: SettingsStartupWindow,
            keywords: SettingsSearchStartupWindowKeywords,
        },
        SearchEntry {
            section: WindowTabs,
            title: SettingsRememberWindowSize,
            keywords: SettingsSearchRememberWindowSizeKeywords,
        },
        SearchEntry {
            section: WindowTabs,
            title: SettingsRestoreLastLayout,
            keywords: SettingsSearchRestoreLastLayoutKeywords,
        },
        SearchEntry {
            section: WindowTabs,
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
            section: WindowTabs,
            title: SettingsNotifyOnCommandFinish,
            keywords: SettingsSearchNotifyOnCommandFinishKeywords,
        },
        SearchEntry {
            section: WindowTabs,
            title: SettingsNotifyThreshold,
            keywords: SettingsSearchNotifyThresholdKeywords,
        },
        SearchEntry {
            section: Keybindings,
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
            title: SettingsSearchHowShellsWorkTitle,
            keywords: SettingsSearchHowShellsWorkKeywords,
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

fn entry_matches(entry: &SearchEntry, query: &str) -> bool {
    t(entry.title).to_lowercase().contains(query)
        || t(entry.keywords).to_lowercase().contains(query)
}

pub(crate) fn section_match_count(section: SettingsSection, query: &str) -> usize {
    settings_search_entries()
        .iter()
        .filter(|e| e.section == section && entry_matches(e, query))
        .count()
}

/// Whether a rendered row is one of the ones the section's `(n)` badge counted.
/// A row can match on its own label, or through the keyword list the search
/// index carries for it — "persist" finds "How shells work" and nothing on that
/// page contains the word.
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

pub(crate) fn total_match_count(query: &str) -> usize {
    SettingsSection::ALL
        .into_iter()
        .map(|s| section_match_count(s, query))
        .sum()
}

pub(crate) fn best_matching_section(query: &str) -> Option<SettingsSection> {
    SettingsSection::ALL
        .into_iter()
        .map(|s| (s, section_match_count(s, query)))
        .filter(|(_, n)| *n > 0)
        .reduce(|best, cur| if cur.1 > best.1 { cur } else { best })
        .map(|(s, _)| s)
}

pub(crate) struct ThemeEditor {
    #[allow(dead_code)]
    pub(crate) for_id: String,
    pub(crate) seed: Vec<(ThemeEdit, Entity<ColorPickerState>)>,
    pub(crate) ansi: Vec<(ThemeEdit, Entity<ColorPickerState>)>,
    pub(crate) image_opacity_slider: Option<Entity<SliderState>>,
    pub(crate) _subs: Vec<Subscription>,
}

pub(crate) struct SettingsState {
    pub(crate) focus_handle: gpui::FocusHandle,
    pub(crate) section: SettingsSection,
    pub(crate) search: Entity<InputState>,
    /// The page's own scroll, and an anchor on it that the first matching row
    /// claims. Searching tells you "Appearance (2)"; these are what carry you
    /// to the two, which on a long page start well below the fold.
    pub(crate) content_scroll: gpui::ScrollHandle,
    pub(crate) ssh_master_scroll: gpui::ScrollHandle,
    pub(crate) ssh_detail_scroll: gpui::ScrollHandle,
    pub(crate) theme_list_scroll: gpui::ScrollHandle,
    pub(crate) search_anchor: gpui::ScrollAnchor,
    /// Set when the query or the section changes, and spent by the next render
    /// that has somewhere to go. A `Cell` because that render only holds `&self`.
    pub(crate) reveal_first_hit: Cell<bool>,
    pub(crate) font_select: Entity<SelectState<SearchableVec<String>>>,
    pub(crate) font_bold_select: Entity<SelectState<SearchableVec<String>>>,
    pub(crate) font_italic_select: Entity<SelectState<SearchableVec<String>>>,
    pub(crate) language_select: Entity<SelectState<SearchableVec<String>>>,
    #[cfg(target_os = "windows")]
    pub(crate) window_backdrop_select: Entity<SelectState<SearchableVec<String>>>,
    pub(crate) shell_program_input: Entity<InputState>,
    pub(crate) shell_args_input: Entity<InputState>,
    pub(crate) wd_path_input: Entity<InputState>,
    pub(crate) link_file_command_input: Entity<InputState>,
    pub(crate) http_proxy_input: Entity<InputState>,
    pub(crate) scroll_slider: Entity<SliderState>,
    pub(crate) window_opacity_slider: Entity<SliderState>,
    pub(crate) theme_editor: Option<ThemeEditor>,
    pub(crate) theme_panel_open: bool,
    pub(crate) theme_panel_slot: ThemeSlot,
    pub(crate) theme_search: Entity<InputState>,
    pub(crate) recording: Option<Recording>,
    pub(crate) rebinding_note: Option<String>,
    pub(crate) ssh_form: Option<SshProfileForm>,
    pub(crate) ssh_detail: SshDetail,
    pub(crate) ssh_filter: Entity<InputState>,
    pub(crate) ssh_collapsed_groups: std::collections::HashSet<String>,
    pub(crate) ssh_quick_connect: Entity<InputState>,
    pub(crate) agent_hooks_host: HostId,
    pub(crate) agent_hooks_states: AgentHooksView,
    pub(crate) agent_hooks_seq: u64,
    pub(crate) agent_hooks_note: Option<(crate::core::agent_hooks::HookAgent, String)>,
    pub(crate) _subs: Vec<Subscription>,
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemeSlot {
    Manual,
    Light,
    Dark,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshDetail {
    None,
    Defaults,
    Profile(Uuid),
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

    show_jump: bool,
    show_forwards: bool,
    show_advanced: bool,

    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    auth: AuthMode,

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
        [&self.name, &self.host, &self.port, &self.user]
            .iter()
            .all(|e| e.read(cx).value().trim().is_empty())
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

    fn is_blank(&self, cx: &App) -> bool {
        [
            &self.bind_host,
            &self.bind_port,
            &self.target_host,
            &self.target_port,
            &self.description,
        ]
        .iter()
        .all(|e| e.read(cx).value().trim().is_empty())
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

/// The inline complaint under a field: one line, in the danger colour, in the
/// column the control sits in. Built before the row rather than inside a
/// `when` closure so it borrows the app for the length of one call.
fn field_error(message: impl Into<String>, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(cx.theme().danger)
        .child(message.into())
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

impl Tty7App {
    pub(crate) fn render_settings(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        // The settings panel covers the whole window. Paint it on an opaque
        // surface so the workspace translucency (window opacity / backdrop
        // material) never shows through the settings UI — while keeping the
        // preset's gradient fill instead of collapsing to a flat color.
        let background: Background = crate::ui::theme::overlay_background(cx);
        // That opaque fill also covers the theme background image the
        // workspace root paints, so the panel carries its own copy (dimmed by
        // the workspace fill, or the settings text would sit straight on the
        // wallpaper); without it the image would blink out for as long as
        // settings is open.
        let background_layers = crate::ui::app::overlay_surface_layers(cx);
        let (foreground, header_muted) = (theme.foreground, theme.muted_foreground);
        let note_bg = theme.secondary.opacity(0.5);

        let (focus_handle, section, theme_panel_open, search) = match self.active_settings() {
            Some(s) => (
                s.focus_handle.clone(),
                s.section,
                s.theme_panel_open,
                s.search.clone(),
            ),
            None => return div(),
        };
        let query = search.read(cx).value().trim().to_lowercase();
        let show_theme_panel = theme_panel_open && section == SettingsSection::Appearance;

        let viewport_w = window.viewport_size().width.as_f32();
        let ui_scale = ui_scale(cx);
        self.settings_viewport_w.set(viewport_w);
        let cols = settings_columns(section, show_theme_panel, viewport_w);
        self.settings_row_width.set(settings_row_width(
            section,
            show_theme_panel,
            viewport_w,
            ui_scale,
        ));
        self.settings_hit_anchored.set(false);

        // A query that matched here has to be reachable, not just counted. The
        // anchor lands on the first matching row below, and `scroll_to` reads
        // where it ended up on the frame after this one — by which time this
        // render has been painted and the anchor knows its own origin.
        //
        // A query that matched nowhere has to be reachable too: the note
        // saying so sits at the top of the page, and a reader who searched
        // from halfway down would otherwise be left with the untouched page
        // the note exists to explain.
        if let Some(s) = self.active_settings()
            && s.reveal_first_hit.get()
        {
            s.reveal_first_hit.set(false);
            let matched_here = section_match_count(section, &query) > 0;
            let matched_nowhere = total_match_count(&query) == 0;
            if !query.is_empty() && (matched_here || matched_nowhere) {
                s.search_anchor.scroll_to(window, cx);
            }
        }

        let prof = crate::ui::perf::enabled()
            .then(|| (std::time::Instant::now(), section.profile_label()));

        let nav_item = |label: &'static str, target: SettingsSection, icon: Icon| {
            let view = cx.entity();
            let count = if query.is_empty() {
                0
            } else {
                section_match_count(target, &query)
            };
            let item = SidebarMenuItem::new(label)
                .icon(icon)
                .active(section == target)
                .on_click(move |_, _window, cx| {
                    view.update(cx, |this, cx| this.select_settings_section(target, cx));
                });
            if count > 0 {
                item.suffix(move |_w, _cx| {
                    div()
                        .text_xs()
                        .text_color(header_muted)
                        .child(format!("({count})"))
                })
            } else {
                item
            }
        };

        let nav_body = SidebarMenu::new()
            .child(nav_item(
                t(L10nKey::SettingsNavAppearance),
                SettingsSection::Appearance,
                Icon::new(IconName::Palette),
            ))
            .child(nav_item(
                t(L10nKey::SettingsNavTerminal),
                SettingsSection::Terminal,
                Icon::new(IconName::SquareTerminal),
            ))
            .child(nav_item(
                t(L10nKey::SettingsNavInput),
                SettingsSection::Input,
                Icon::new(IconName::Settings2),
            ))
            .child(nav_item(
                t(L10nKey::SettingsNavSsh),
                SettingsSection::Ssh,
                Icon::new(IconName::Globe),
            ))
            .child(nav_item(
                t(L10nKey::SettingsNavAgents),
                SettingsSection::Agents,
                Icon::new(IconName::Bot),
            ))
            .child(nav_item(
                t(L10nKey::SettingsNavWindowTabs),
                SettingsSection::WindowTabs,
                Icon::new(IconName::WindowRestore),
            ))
            .child(nav_item(
                t(L10nKey::SettingsNavKeybindings),
                SettingsSection::Keybindings,
                Icon::new(IconName::CaseSensitive),
            ))
            .child(nav_item(
                t(L10nKey::SettingsNavAbout),
                SettingsSection::About,
                Icon::empty().path("icons/circle-info.svg"),
            ));

        let sidebar = Sidebar::new("settings-sidebar")
            .collapsible(SidebarCollapsible::None)
            .w(px(cols.nav))
            .header(
                v_flex()
                    .w_full()
                    .px_2()
                    .gap_2()
                    .pt(px(crate::ui::app::TITLE_BAR_HEIGHT))
                    .pb_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(header_muted)
                            .child(t(L10nKey::SettingsHeader)),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                Icon::empty()
                                    .path("stock/icons/search.svg")
                                    .size(px(16.))
                                    .text_color(header_muted),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(&search).appearance(false).pl_0()),
                            ),
                    ),
            )
            .child(nav_body);

        let content = match section {
            SettingsSection::Appearance => self.render_settings_appearance(cx),
            SettingsSection::Terminal => self.render_settings_terminal(cx),
            SettingsSection::Input => self.render_settings_input(cx),
            SettingsSection::Ssh => self.render_settings_ssh(cx),
            SettingsSection::Agents => self.render_settings_agents(cx),
            SettingsSection::WindowTabs => self.render_settings_window_tabs(cx),
            SettingsSection::Keybindings => self.render_settings_keybindings(cx),
            SettingsSection::About => self.render_settings_about(cx),
        };

        // A query that matches nothing anywhere leaves the nav badge-less and
        // `autoselect_settings_search` with nowhere to go, so without this the
        // page just sits there looking like the search did nothing.
        let no_match_note = (!query.is_empty() && total_match_count(&query) == 0).then(|| {
            div()
                .id("settings-no-match")
                .anchor_scroll(self.active_settings().map(|s| s.search_anchor.clone()))
                .mb_6()
                .px_3()
                .py_2()
                .rounded_lg()
                .bg(note_bg)
                .text_sm()
                .text_color(header_muted)
                .child(t_fmt(
                    L10nKey::SettingsNothingMatches,
                    &[("query", query.as_str())],
                ))
        });

        // No fill of its own: the root already paints the opaque surface and
        // the background image behind it, and repainting here would hide the
        // image again in the one pane that fills most of the panel.
        //
        // `min_w_0` and not a `CONTENT_MIN_W` floor: `settings_columns` sized
        // the chrome so this pane clears it, and a floor a flex row cannot
        // honour does not push the nav back — it overflows, and overflow here
        // means content painted off the edge of the window, which is the other
        // half of the bug this file is fixing.
        let content_pane = if section == SettingsSection::Ssh {
            v_flex()
                .id("settings-content")
                .flex_1()
                .min_w_0()
                .h_full()
                .child(content)
                .into_any_element()
        } else {
            let body = v_flex()
                .id("settings-content")
                .size_full()
                .overflow_y_scroll()
                .when_some(self.active_settings(), |pane, s| {
                    pane.track_scroll(&s.content_scroll)
                })
                .child(
                    // The padding box needs its own width for the reading
                    // column's `w_full` to resolve against something definite;
                    // without it the percentage falls back to the content, and
                    // `max_w` loses to a row that measures wider — which is how
                    // the theme card came to run the width of the window on the
                    // Chinese and Japanese pages while every other row stopped
                    // at the column.
                    div().w_full().px_10().py_8().child(
                        div()
                            .w_full()
                            .max_w(px(READING_COLUMN * ui_scale))
                            .children(no_match_note)
                            .child(content),
                    ),
                );
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                // No fill here either: the root paints it, and a second one on
                // the pane that covers most of the page would hide the theme
                // image behind it.
                .when_some(self.active_settings(), |pane, s| {
                    pane.child(crate::ui::scrollbar::with_vertical_scrollbar(
                        "settings-content-scrollbar",
                        body,
                        &s.content_scroll,
                    ))
                })
                .into_any_element()
        };

        let root = div()
            .size_full()
            .relative()
            .flex()
            .flex_row()
            .bg(background)
            .text_color(foreground)
            .track_focus(&focus_handle)
            // Escape peels one layer at a time. With the theme picker open that
            // layer is the picker: closing the whole page instead threw away a
            // panel the user had opened a moment ago, and left them to walk
            // back to Appearance to try again.
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key.as_str() != "escape" {
                    return;
                }
                if show_theme_panel {
                    this.close_theme_panel(window, cx);
                    return;
                }
                this.close_settings_checked(window, cx);
            }))
            .children(background_layers)
            .child(sidebar)
            .child(content_pane)
            .child(
                crate::ui::app::window_move_gesture(
                    div()
                        .id("settings-titlebar-drag")
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(crate::ui::app::TITLE_BAR_HEIGHT)),
                    "settings-titlebar-drag",
                    window,
                    cx,
                )
                .on_double_click(|_, window, _| window.titlebar_double_click()),
            )
            // Beside the page while there is room for both, over it when there
            // is not. As a column it was taking its 300px from the page and
            // from nothing else, which is how a half-width window ended up
            // rendering a description one character wide.
            .when(show_theme_panel && !cols.panel_overlays, |r| {
                r.child(self.render_theme_panel(cx))
            })
            .when(show_theme_panel && cols.panel_overlays, |r| {
                r.child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .occlude()
                        .shadow_lg()
                        .child(self.render_theme_panel(cx)),
                )
            })
            .when(!show_theme_panel, |r| {
                r.child(
                    div()
                        .absolute()
                        .top(px((TITLE_BAR_HEIGHT - TILE_SIZE) / 2.))
                        .right(px(10.))
                        .occlude()
                        .child(
                            Button::new("settings-close")
                                .icon(Icon::new(IconName::Close))
                                .ghost()
                                .with_size(px(
                                    TILE_GLYPH_LINE / crate::ui::tab_strip::BUTTON_ICON_SCALE
                                ))
                                .w(px(TILE_SIZE))
                                .h(px(TILE_SIZE))
                                .rounded_lg()
                                .tooltip(t(L10nKey::Close))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_settings_checked(window, cx)
                                })),
                        ),
                )
            });

        if let Some((start, label)) = prof {
            crate::ui::perf::record(label, start.elapsed());
        }
        root
    }

    /// The column widths this render settled on. `settings_columns` is pure and
    /// cheap, so the two pages that draw chrome of their own work them out
    /// again rather than have the answer threaded through every builder.
    fn settings_columns_now(&self) -> SettingsColumns {
        let (section, panel_open) = match self.active_settings() {
            Some(s) => (
                s.section,
                s.theme_panel_open && s.section == SettingsSection::Appearance,
            ),
            None => (SettingsSection::Appearance, false),
        };
        settings_columns(section, panel_open, self.settings_viewport_w.get())
    }

    /// Whether the row measured this render came out narrower than a threshold
    /// quoted at the default interface font — the only way those px thresholds
    /// mean anything to a reader who scaled the interface up.
    fn settings_row_under(&self, at_default_font: f32, cx: &App) -> bool {
        self.settings_row_width.get() < at_default_font * ui_scale(cx)
    }

    fn header_text(&self, title: &str, cx: &Context<Self>) -> Div {
        div()
            .text_base()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().foreground)
            .child(title.to_string())
    }

    /// A heading *inside* a section — quieter than `section_header`, for
    /// breaking a long run of rows into groups you can scan.
    fn subgroup_header(&self, key: L10nKey, cx: &Context<Self>) -> Div {
        div()
            .pt_4()
            .pb_1()
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(cx.theme().muted_foreground)
            .child(t(key))
    }

    /// The scroll anchor for the first thing on the page the query matched,
    /// whatever kind of element that is. A section header can be the only
    /// match on its page — "ansi", "how shells work" — and while the dimming
    /// around it already picks it out, nothing was carrying the page to it:
    /// search "ansi" from the bottom of Appearance and every row greys out
    /// with the one answer left above the fold.
    fn first_hit_anchor(&self, label: &str, cx: &Context<Self>) -> Option<gpui::ScrollAnchor> {
        let s = self.active_settings()?;
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

    pub(crate) fn section_header(&self, title: &str, cx: &Context<Self>) -> Stateful<Div> {
        self.header_text(title, cx)
            .mb_4()
            .id(settings_header_id(title))
            .anchor_scroll(self.first_hit_anchor(title, cx))
    }

    fn section_intro(
        &self,
        title: &str,
        desc: impl Into<String>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        v_flex()
            .mb_4()
            .gap_1()
            .id(settings_header_id(title))
            .anchor_scroll(self.first_hit_anchor(title, cx))
            .child(self.header_text(title, cx))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(desc.into()),
            )
    }

    pub(crate) fn section_rule(&self, cx: &Context<Self>) -> Div {
        div().h(px(1.)).my_7().bg(cx.theme().border)
    }

    pub(crate) fn settings_row(
        &self,
        label: impl Into<String>,
        desc: impl Into<String>,
        control: AnyElement,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme();
        let label = label.into();
        let desc = desc.into();
        // Descriptions can contain live status (for example an agent hook target), so they
        // must not participate in the identity that preserves GPUI's hover state.
        let element_id = settings_row_id(&label, &desc);
        // The nav badge says "Appearance (2)"; this is what makes those two
        // findable once you are on the page. Only mark rows when the section
        // actually holds a match — otherwise a query that landed elsewhere
        // would grey out a page the user is simply reading.
        let (hit, miss) = match self.active_settings() {
            Some(s) => {
                let query = s.search.read(cx).value().trim().to_lowercase();
                match query.is_empty() || section_match_count(s.section, &query) == 0 {
                    true => (false, false),
                    false => {
                        let hit = row_matches_query(s.section, &label, &query);
                        (hit, !hit)
                    }
                }
            }
            None => (false, false),
        };
        let first_hit_anchor = self.first_hit_anchor(&label, cx);
        // The control never shrinks, so on a narrow pane it takes the width and
        // the label column — which must keep `min_w_0` or long descriptions
        // stop wrapping — is squeezed to a letter per line. Below the width
        // where both still fit, put the control on its own line instead.
        // Measured, not `flex_wrap`: wrapping made the label column size to its
        // description, which then ran out past the row on every wide page.
        let stacked = self.settings_row_under(STACK_ROW_BELOW, cx);
        let labels = v_flex()
            .gap_0p5()
            .min_w_0()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(label),
            )
            .when(!desc.is_empty(), |col| {
                col.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(desc),
                )
            });
        div()
            .id(element_id)
            .flex()
            .when(stacked, |row| row.flex_col().items_start().gap_2())
            .when(!stacked, |row| {
                row.flex_row().items_center().justify_between().gap_8()
            })
            .py_2()
            .px_2p5()
            .mx_neg_2p5()
            .rounded_lg()
            .when(hit, |row| row.bg(theme.accent.opacity(0.16)))
            // Only the first hit on the page carries the anchor: it is the one
            // the page scrolls to, and a later row claiming it would drag the
            // view past the matches above.
            .anchor_scroll(first_hit_anchor)
            .when(miss, |row| row.opacity(0.45))
            .hover(|h| h.bg(gpui::rgb(cx.global::<presets::Surfaces>().window.hover)))
            .on_hover(cx.listener(|_this, _hovered, _window, cx| cx.notify()))
            .child(labels)
            // Stacked, the control column takes the row: that is what gives a
            // `max_w_full` control a definite width to shrink against, and on
            // the SSH page the widest of them is 260 in a column that can be
            // `CONTENT_MIN_W`.
            .child(
                h_flex()
                    .when(stacked, |c| c.w_full())
                    .when(!stacked, |c| c.flex_shrink_0())
                    .child(control),
            )
    }

    pub(crate) fn segmented(
        &self,
        id: impl Into<SharedString>,
        options: &[&str],
        selected: usize,
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Self, usize, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let sf = cx.global::<presets::Surfaces>().window;
        self.segmented_on(sf, id, options, selected, cx, on_pick)
    }

    pub(crate) fn segmented_on(
        &self,
        sf: presets::Surface,
        id: impl Into<SharedString>,
        options: &[&str],
        selected: usize,
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Self, usize, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let border = cx.theme().border;
        let id: SharedString = id.into();
        let on_pick = std::rc::Rc::new(on_pick);
        let count = options.len();
        h_flex()
            .id(gpui::ElementId::Name(id.clone()))
            .h(px(24.))
            .rounded(rounding::TRACK_RADIUS)
            .border_1()
            .border_color(border)
            .bg(gpui::rgb(sf.base))
            .overflow_hidden()
            .children(options.iter().enumerate().map(|(i, label)| {
                let active = i == selected;
                let on_pick = on_pick.clone();
                let corners =
                    rounding::segment_corners(i, count, rounding::TRACK_RADIUS, rounding::HAIRLINE);
                h_flex()
                    .id(gpui::ElementId::NamedInteger(id.clone(), i as u64))
                    .items_center()
                    .justify_center()
                    .h_full()
                    .px_2p5()
                    .text_sm()
                    .cursor_pointer()
                    .rounded_corners(corners)
                    .when(i > 0, |s| s.border_l_1().border_color(border))
                    .when(active, |s| {
                        s.bg(gpui::rgb(sf.selected))
                            .text_color(gpui::rgb(sf.text_selected))
                            .font_weight(FontWeight::MEDIUM)
                    })
                    .when(!active, |s| {
                        s.text_color(gpui::rgb(sf.text_resting))
                            .hover(|h| h.bg(gpui::rgb(sf.hover)))
                    })
                    .active(|s| s.bg(gpui::rgb(sf.pressed)))
                    .child(label.to_string())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        on_pick(this, i, window, cx);
                    }))
            }))
            .into_any_element()
    }

    fn render_settings_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let foreground = theme.foreground;
        let border = theme.border;
        let hover_bg = gpui::rgb(cx.global::<presets::Surfaces>().window.hover);
        let stepper_bg = theme.secondary.opacity(0.35);
        let font_size = self.font_size;
        let (font_select, font_bold_select, font_italic_select, language_select) =
            match self.active_settings() {
                Some(s) => (
                    s.font_select.clone(),
                    s.font_bold_select.clone(),
                    s.font_italic_select.clone(),
                    s.language_select.clone(),
                ),
                None => return div().into_any_element(),
            };
        let cfg = cx.global::<Config>();
        let cursor_style = cfg.cursor_style;
        let cursor_blink = cfg.cursor_blink;
        let font_ligatures = cfg.font_features.as_ref().is_some_and(|features| {
            features.is_calt_enabled() == Some(true)
                || features
                    .tag_value_list()
                    .iter()
                    .any(|(tag, value)| tag == "liga" && *value != 0)
        });

        let step = move |id: &'static str, glyph: &'static str, slot: usize| {
            let corners =
                rounding::segment_corners(slot, 3, rounding::TRACK_RADIUS, rounding::HAIRLINE);
            h_flex()
                .id(id)
                .items_center()
                .justify_center()
                .h_full()
                .px_2p5()
                .text_sm()
                .cursor_pointer()
                .text_color(foreground)
                .when(slot > 0, |s| s.border_l_1().border_color(border))
                .rounded_corners(corners)
                .hover(|h| h.bg(hover_bg))
                .child(glyph)
        };
        let control_h = px(24.);
        let stepper_row =
            move |dec: Stateful<Div>, value: String, inc: Stateful<Div>, reset: Button| {
                h_flex()
                    .items_center()
                    .gap_3()
                    .child(reset)
                    .child(
                        h_flex()
                            .items_center()
                            .h(control_h)
                            .rounded(rounding::TRACK_RADIUS)
                            .bg(stepper_bg)
                            .border_1()
                            .border_color(border)
                            .overflow_hidden()
                            .child(dec)
                            .child(
                                div()
                                    .min_w(px(40.))
                                    .border_l_1()
                                    .border_color(border)
                                    .py_1()
                                    .text_center()
                                    .text_sm()
                                    .text_color(foreground)
                                    .child(value),
                            )
                            .child(inc),
                    )
                    .into_any_element()
            };
        let font_size_control = stepper_row(
            step("font-dec", "−", 0).on_click(
                cx.listener(|this, _, _w, cx| this.change_font_size(-FONT_SIZE_STEP, cx)),
            ),
            format!("{:.0}", font_size),
            step("font-inc", "+", 2)
                .on_click(cx.listener(|this, _, _w, cx| this.change_font_size(FONT_SIZE_STEP, cx))),
            Button::new("font-reset")
                .label(t(L10nKey::Reset))
                .ghost()
                .small()
                .on_click(cx.listener(|this, _, _w, cx| this.reset_font_size(cx))),
        );

        let ui_font_size = self.ui_font_size(cx);
        let ui_font_size_control = stepper_row(
            step("ui-font-dec", "−", 0).on_click(
                cx.listener(|this, _, _w, cx| this.change_ui_font_size(-UI_FONT_SIZE_STEP, cx)),
            ),
            format!("{ui_font_size:.0}"),
            step("ui-font-inc", "+", 2).on_click(
                cx.listener(|this, _, _w, cx| this.change_ui_font_size(UI_FONT_SIZE_STEP, cx)),
            ),
            Button::new("ui-font-reset")
                .label(t(L10nKey::Reset))
                .ghost()
                .small()
                .on_click(cx.listener(|this, _, _w, cx| this.reset_ui_font_size(cx))),
        );

        let line_height = self.line_height;
        let line_height_control = stepper_row(
            step("lh-dec", "−", 0).on_click(
                cx.listener(|this, _, _w, cx| this.change_line_height(-LINE_HEIGHT_STEP, cx)),
            ),
            format!("{:.2}", line_height),
            step("lh-inc", "+", 2).on_click(
                cx.listener(|this, _, _w, cx| this.change_line_height(LINE_HEIGHT_STEP, cx)),
            ),
            Button::new("lh-reset")
                .label(t(L10nKey::Reset))
                .ghost()
                .small()
                .on_click(cx.listener(|this, _, _w, cx| this.reset_line_height(cx))),
        );

        let font_dropdown = |state: &Entity<SelectState<SearchableVec<String>>>| {
            Select::new(state)
                .small()
                .w(px(180.))
                .h(control_h)
                .search_placeholder(crate::ui::i18n::t(crate::ui::i18n::L10nKey::SearchFonts))
                .menu_max_h(px(224.))
                .into_any_element()
        };
        let font_family_control = font_dropdown(&font_select);
        let font_bold_control = font_dropdown(&font_bold_select);
        let font_italic_control = font_dropdown(&font_italic_select);
        let ligature_switch = crate::ui::theme::switch("font-ligatures", cx)
            .checked(font_ligatures)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_font_ligatures(*on, cx)))
            .into_any_element();
        let language_control = Select::new(&language_select)
            .small()
            .w(px(180.))
            .h(control_h)
            .menu_max_h(px(224.))
            .into_any_element();

        let cursor_idx = match cursor_style {
            CursorStyle::Block => 0,
            CursorStyle::Bar => 1,
            CursorStyle::Underline => 2,
        };
        let cursor_style_control = self.segmented(
            "cursor-style",
            &["Block", "Bar", "Underline"],
            cursor_idx,
            cx,
            |this, ix, _w, cx| {
                let style = match ix {
                    0 => CursorStyle::Block,
                    1 => CursorStyle::Bar,
                    _ => CursorStyle::Underline,
                };
                this.set_cursor_style(style, cx);
            },
        );
        let blink_switch = crate::ui::theme::switch("cursor-blink", cx)
            .checked(cursor_blink)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_cursor_blink(*on, cx)))
            .into_any_element();

        v_flex()
            .child(self.section_intro(
                t(L10nKey::SettingsThemeIntroTitle),
                t(L10nKey::SettingsThemeIntroDesc),
                cx,
            ))
            .child(self.render_theme_selection(cx))
            .child(self.render_custom_themes(cx))
            .child(self.section_rule(cx))
            .child(self.render_window_section(cx))
            .child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsLanguage), cx))
            .child(self.settings_row(
                t(L10nKey::SettingsLanguage),
                t(L10nKey::SettingsLanguageDesc),
                language_control,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsTypography), cx))
            .child(self.settings_row(
                t(L10nKey::SettingsFontSize),
                t(L10nKey::SettingsFontSizeDesc),
                font_size_control,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsUiFontSize),
                t(L10nKey::SettingsUiFontSizeDesc),
                ui_font_size_control,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsLineHeight),
                t(L10nKey::SettingsLineHeightDesc),
                line_height_control,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsFontFamily),
                t(L10nKey::SettingsFontFamilyDesc),
                font_family_control,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsBoldFont),
                t(L10nKey::SettingsBoldFontDesc),
                font_bold_control,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsItalicFont),
                t(L10nKey::SettingsItalicFontDesc),
                font_italic_control,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsFontLigatures),
                t(L10nKey::SettingsFontLigaturesDesc),
                ligature_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsCursor), cx))
            .child(self.settings_row(
                t(L10nKey::SettingsCursorShape),
                t(L10nKey::SettingsCursorShapeDesc),
                cursor_style_control,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsCursorBlink),
                t(L10nKey::SettingsCursorBlinkDesc),
                blink_switch,
                cx,
            ))
            .into_any_element()
    }

    fn render_window_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(slider) = self
            .active_settings()
            .map(|s| s.window_opacity_slider.clone())
        else {
            return div().into_any_element();
        };
        let config = cx.global::<Config>();
        let overridden = window_overrides_active(config, cfg!(target_os = "windows"));
        let dim_inactive_panes = config.dim_inactive_panes;
        let opacity = Tty7App::effective_window_opacity(cx);

        let opacity_control = h_flex()
            .items_center()
            .gap_3()
            .w(px(240.))
            .max_w_full()
            .child(div().flex_1().child(Slider::new(&slider)))
            .child(
                div()
                    .w(px(38.))
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_right()
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .child(format!("{:.0}%", opacity * 100.)),
            )
            .into_any_element();
        // Windows exposes the native backdrop materials directly; macOS keeps
        // the simple blur toggle, which drives its vibrancy.
        #[cfg(target_os = "windows")]
        let blur_control = {
            // Both selects come from the same SettingsState resolved at the
            // top of this function (window_opacity_slider), so the None arm
            // is unreachable today; fall back to an empty control rather
            // than returning from the whole section — a missing select must
            // never silently drop the opacity slider and the rest.
            match self
                .active_settings()
                .map(|s| s.window_backdrop_select.clone())
            {
                Some(select) => Select::new(&select)
                    .small()
                    .w(px(180.))
                    .h(px(24.))
                    .menu_max_h(px(224.))
                    .into_any_element(),
                None => div().into_any_element(),
            }
        };
        #[cfg(not(target_os = "windows"))]
        let blur_control =
            {
                let theme = presets::by_id(cx, &crate::ui::theme::effective_preset_id(cx));
                let blur = config.window_blur.unwrap_or(theme.blur);
                crate::ui::theme::switch("window-blur", cx)
                    .checked(blur)
                    .on_click(cx.listener(|this, on: &bool, window, cx| {
                        this.set_window_blur(*on, window, cx)
                    }))
                    .into_any_element()
            };
        // `Auto` is the one backdrop that still defers to the legacy blur
        // flag, which is shared with the other platforms' vibrancy switch and
        // travels with a synced config. Offer that switch here exactly when it
        // has an effect — otherwise a stored `window_blur: true` would blur
        // the window with no visible control to clear it, short of the reset
        // button, which also discards the user's opacity.
        #[cfg(target_os = "windows")]
        let auto_blur_row = (config.window_backdrop == WindowBackdrop::Auto).then(|| {
            let theme = presets::by_id(cx, &crate::ui::theme::effective_preset_id(cx));
            let blur = config.window_blur.unwrap_or(theme.blur);
            let control =
                crate::ui::theme::switch("window-blur", cx)
                    .checked(blur)
                    .on_click(cx.listener(|this, on: &bool, window, cx| {
                        this.set_window_blur(*on, window, cx)
                    }))
                    .into_any_element();
            self.settings_row(
                t(L10nKey::SettingsBlur),
                // Not `SettingsBlurDesc` — that one says "(macOS)", which is
                // exactly wrong here. This row explains the flag's one
                // remaining job on Windows: feeding the `Auto` material.
                t(L10nKey::SettingsBlurAutoDesc),
                control,
                cx,
            )
        });
        #[cfg(not(target_os = "windows"))]
        let auto_blur_row: Option<Stateful<Div>> = None;
        let dim_switch = crate::ui::theme::switch("dim-inactive-panes", cx)
            .checked(dim_inactive_panes)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_dim_inactive_panes(*on, cx)))
            .into_any_element();

        v_flex()
            .child(self.section_header(t(L10nKey::SettingsTransparency), cx))
            .child(self.settings_row(
                t(L10nKey::SettingsOpacity),
                t(L10nKey::SettingsOpacityDesc),
                opacity_control,
                cx,
            ))
            .child(self.settings_row(
                t(if cfg!(target_os = "windows") {
                    L10nKey::SettingsBackdrop
                } else {
                    L10nKey::SettingsBlur
                }),
                t(if cfg!(target_os = "windows") {
                    L10nKey::SettingsBackdropDesc
                } else {
                    L10nKey::SettingsBlurDesc
                }),
                blur_control,
                cx,
            ))
            .children(auto_blur_row)
            .when(overridden, |this| {
                this.child(
                    h_flex().mt_2().child(
                        Button::new("follow-theme-window")
                            .label(t(L10nKey::FollowTheme))
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reset_window_overrides(window, cx)
                            })),
                    ),
                )
            })
            .child(self.settings_row(
                t(L10nKey::SettingsDimInactivePanes),
                t(L10nKey::SettingsDimInactivePanesDesc),
                dim_switch,
                cx,
            ))
            .into_any_element()
    }

    fn render_custom_themes(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self.active_settings().and_then(|s| s.theme_editor.as_ref());

        let folder_button = Button::new("open-themes-folder")
            .label(t(L10nKey::SettingsOpenThemesFolder))
            .small()
            .on_click(cx.listener(|this, _, w, cx| this.open_themes_folder(w, cx)));

        if let Some(editor) = editor {
            let label_of = |&(edit, ref state): &(ThemeEdit, Entity<ColorPickerState>)| {
                (
                    crate::ui::app::theme_edit_label(edit).to_string(),
                    state.clone(),
                )
            };
            let seed: Vec<_> = editor.seed.iter().map(label_of).collect();
            let ansi: Vec<_> = editor.ansi.iter().map(label_of).collect();
            let image_opacity_slider = editor.image_opacity_slider.clone();

            let theme = presets::by_id(cx, &crate::ui::theme::effective_preset_id(cx));
            let image = theme.image.clone();
            let image_name = image.as_ref().map(|i| {
                i.path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| i.path.display().to_string())
            });
            let image_control = h_flex()
                .items_center()
                .gap_2()
                .w(px(240.))
                .child(
                    Button::new("pick-theme-image")
                        .label(if image.is_some() {
                            t(L10nKey::SettingsChangeThemeImage)
                        } else {
                            t(L10nKey::SettingsChooseThemeImage)
                        })
                        .small()
                        .on_click(cx.listener(|this, _, _w, cx| this.pick_theme_image(cx))),
                )
                .when_some(image_name, |this, name| {
                    this.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(name),
                    )
                    .child(
                        Button::new("remove-theme-image")
                            .label(t(L10nKey::SettingsRemoveThemeImage))
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.remove_theme_image(window, cx)
                            })),
                    )
                })
                .into_any_element();
            let image_opacity_row = image_opacity_slider.map(|slider| {
                let readout = image.as_ref().map(|i| i.opacity).unwrap_or(0.3);
                let control = h_flex()
                    .items_center()
                    .gap_3()
                    .w(px(240.))
                    .child(div().flex_1().child(Slider::new(&slider)))
                    .child(
                        div()
                            .w(px(38.))
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .text_right()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(format!("{:.0}%", readout * 100.)),
                    )
                    .into_any_element();
                self.settings_row(
                    t(L10nKey::SettingsImageOpacity),
                    t(L10nKey::SettingsImageOpacityDesc),
                    control,
                    cx,
                )
            });

            return v_flex()
                .mt_5()
                .child(self.section_intro(
                    t(L10nKey::SettingsEditTheme),
                    t(L10nKey::SettingsEditThemeIntro),
                    cx,
                ))
                .children(
                    seed.into_iter()
                        .map(|(label, state)| self.render_theme_color_row(label, state, cx)),
                )
                .child(self.settings_row(
                    t(L10nKey::SettingsBackgroundImage),
                    t(L10nKey::SettingsBackgroundImageDesc),
                    image_control,
                    cx,
                ))
                .children(image_opacity_row)
                .child(self.section_header(t(L10nKey::SettingsAnsiColors), cx))
                .children(
                    ansi.into_iter()
                        .map(|(label, state)| self.render_theme_color_row(label, state, cx)),
                )
                .child(h_flex().mt_4().child(folder_button))
                .into_any_element();
        }

        v_flex()
            .mt_5()
            .child(self.section_intro(
                t(L10nKey::SettingsCustomThemes),
                t(L10nKey::SettingsCustomThemesIntro),
                cx,
            ))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Button::new("duplicate-theme")
                            .label(t(L10nKey::SettingsDuplicateToEdit))
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.fork_active_theme(window, cx)
                            })),
                    )
                    .child(folder_button),
            )
            .into_any_element()
    }

    fn render_theme_color_row(
        &self,
        label: String,
        state: Entity<ColorPickerState>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let control = ColorPicker::new(&state).small().into_any_element();
        self.settings_row(label, "", control, cx)
    }

    fn render_settings_ssh(&self, cx: &mut Context<Self>) -> AnyElement {
        let border = cx.theme().border;
        let Some((master_scroll, detail_scroll)) = self
            .active_settings()
            .map(|s| (s.ssh_master_scroll.clone(), s.ssh_detail_scroll.clone()))
        else {
            return div().into_any_element();
        };
        let master = v_flex()
            .id("ssh-master")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&master_scroll)
            .child(self.render_ssh_master(cx));
        let detail = v_flex()
            .id("ssh-detail")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&detail_scroll)
            .child(
                div()
                    .pt(px(crate::ui::app::TITLE_BAR_HEIGHT))
                    .px_8()
                    .pb_8()
                    .child(
                        div()
                            .w_full()
                            .max_w(px(720.))
                            .child(self.render_ssh_detail(cx)),
                    ),
            );
        h_flex()
            .size_full()
            .items_start()
            .child(
                v_flex()
                    .flex_shrink_0()
                    .w(px(self.settings_columns_now().ssh_list))
                    .h_full()
                    .border_r_1()
                    .border_color(border)
                    .child(crate::ui::scrollbar::with_vertical_scrollbar(
                        "ssh-master-scrollbar",
                        master,
                        &master_scroll,
                    )),
            )
            .child(v_flex().flex_1().min_w_0().h_full().child(
                crate::ui::scrollbar::with_vertical_scrollbar(
                    "ssh-detail-scrollbar",
                    detail,
                    &detail_scroll,
                ),
            ))
            .into_any_element()
    }

    fn render_ssh_master(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let sf = cx.global::<presets::Surfaces>().window;
        let profiles = cx.global::<Config>().ssh_profiles.clone();
        let (filter, collapsed, detail) = match self.active_settings() {
            Some(s) => (
                s.ssh_filter.clone(),
                s.ssh_collapsed_groups.clone(),
                s.ssh_detail,
            ),
            None => return div().into_any_element(),
        };
        let query = filter.read(cx).value().trim().to_lowercase();
        let live = self.live_ssh_profiles(cx);
        let menu_app = cx.entity().downgrade();

        let header = v_flex()
            .gap_2()
            .child(self.header_text(t(L10nKey::SettingsHosts), cx))
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Icon::empty()
                            .path("stock/icons/search.svg")
                            .size(px(16.))
                            .text_color(muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&filter).appearance(false).pl_0()),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_0p5()
                            .child(
                                Button::new("ssh-profiles-add")
                                    .icon(Icon::new(IconName::Plus))
                                    .ghost()
                                    .small()
                                    .tooltip(t(L10nKey::SettingsNewHost))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.add_new_profile(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("ssh-profiles-more")
                                    .icon(Icon::empty().path("stock/icons/ellipsis.svg"))
                                    .ghost()
                                    .small()
                                    .tooltip(t(L10nKey::TabTooltipMore))
                                    .dropdown_menu_with_anchor(
                                        gpui::Anchor::TopRight,
                                        move |menu, _window, _cx| {
                                            Self::ssh_master_menu(menu, &menu_app)
                                        },
                                    ),
                            ),
                    ),
            );

        let mut groups: Vec<(String, Vec<SshProfile>)> = Vec::new();
        for p in profiles.iter().filter(|p| ssh_row_matches(p, &query)) {
            let key = ssh_group_key(p).to_string();
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, bucket)) => bucket.push(p.clone()),
                None => groups.push((key, vec![p.clone()])),
            }
        }
        groups.sort_by(|a, b| {
            ssh_group_rank(&a.0)
                .cmp(&ssh_group_rank(&b.0))
                .then_with(|| a.0.cmp(&b.0))
        });

        let mut list = v_flex().gap_0p5().w_full().child(self.render_ssh_row(
            "ssh-defaults-row",
            t(L10nKey::SettingsDefaults),
            t(L10nKey::SettingsInheritedByEveryHost),
            detail == SshDetail::Defaults,
            None,
            sf,
            cx.listener(|this, _, _w, cx| this.select_ssh_defaults(cx)),
            None,
            cx,
        ));

        if profiles.is_empty() {
            list = list.child(
                div()
                    .py_4()
                    // px_2 is what `render_ssh_row` insets its title by: a note
                    // standing in for the rows starts on their column, not on
                    // the list's own edge.
                    .px_2()
                    .text_sm()
                    .text_color(muted)
                    .child(t(L10nKey::SettingsNoSavedHosts)),
            );
        } else if groups.is_empty() {
            list = list.child(
                div()
                    .py_4()
                    .px_2()
                    .text_sm()
                    .text_color(muted)
                    .child(t_fmt(L10nKey::SettingsNothingMatches, &[("query", &query)])),
            );
        }

        for (key, bucket) in groups {
            let is_collapsed = query.is_empty() && collapsed.contains(&key);
            let live_here = bucket.iter().filter(|p| live.contains(&p.id)).count();
            list = list.child(self.render_ssh_group_header(
                &key,
                bucket.len(),
                is_collapsed,
                live_here,
                cx,
            ));
            if is_collapsed {
                continue;
            }
            for p in &bucket {
                list = list.child(self.render_ssh_host_row(
                    p,
                    detail == SshDetail::Profile(p.id),
                    live.contains(&p.id),
                    sf,
                    cx,
                ));
            }
        }

        v_flex()
            .p_2()
            .gap_2()
            .pt(px(crate::ui::app::TITLE_BAR_HEIGHT))
            .child(header)
            .child(list)
            .into_any_element()
    }

    fn ssh_master_menu(menu: PopupMenu, app: &gpui::WeakEntity<Self>) -> PopupMenu {
        menu.min_w(px(200.))
            .item(
                PopupMenuItem::new(t(L10nKey::SettingsImportFromSshConfig)).on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let _ =
                            app.update(cx, |this, cx| this.import_ssh_config_profiles(window, cx));
                    }
                }),
            )
            .item(
                PopupMenuItem::new(t(L10nKey::SettingsExpandAllGroups)).on_click({
                    let app = app.clone();
                    move |_, _window, cx| {
                        let _ = app.update(cx, |this, cx| {
                            if let Some(s) = this.active_settings_mut() {
                                s.ssh_collapsed_groups.clear();
                            }
                            cx.notify();
                        });
                    }
                }),
            )
    }

    fn render_ssh_group_header(
        &self,
        key: &str,
        count: usize,
        collapsed: bool,
        live_here: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let sf = cx.global::<presets::Surfaces>().window;
        let owned_key = key.to_string();
        let chevron = if collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };
        h_flex()
            .id(SharedString::from(format!("ssh-group-{key}")))
            .items_center()
            .gap_1()
            .w_full()
            .mt_2()
            .py_1()
            // 8 + 10 + 4 puts the group name on the same column as a host
            // title, which sits 8 + 6 + 8 past the list edge — and it hands
            // the header the same 8px inset the rows hover with.
            .px_2()
            .rounded_md()
            .cursor_pointer()
            .text_xs()
            .text_color(muted)
            .hover(|s| s.bg(gpui::rgb(sf.hover)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _w, cx| {
                    cx.stop_propagation();
                    this.toggle_ssh_group(owned_key.clone(), cx);
                }),
            )
            .child(Icon::new(chevron).size(px(10.)))
            .child(div().truncate().child(ssh_group_label(key).to_string()))
            .child(div().child(format!("· {count}")))
            .child(div().flex_1())
            .when(collapsed && live_here > 0, |row| {
                row.child(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .child(div().size(px(5.)).rounded_full().bg(cx.theme().success))
                        .child(div().child(live_here.to_string())),
                )
            })
            .into_any_element()
    }

    fn render_ssh_host_row(
        &self,
        p: &SshProfile,
        selected: bool,
        live: bool,
        sf: presets::Surface,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = p.id;
        let row_idx = id.as_u128() as usize;
        let subtitle = to_connect_string(p);
        let title = if p.name.is_empty() {
            subtitle.clone()
        } else {
            p.name.clone()
        };
        self.render_ssh_row(
            SharedString::from(format!("ssh-profile-row-{row_idx}")),
            title,
            subtitle,
            selected,
            Some(live),
            sf,
            cx.listener(move |this, _, window, cx| {
                if let Some(profile) = cx
                    .global::<Config>()
                    .ssh_profiles
                    .iter()
                    .find(|p| p.id == id)
                    .cloned()
                {
                    this.ssh_form_load(&profile, window, cx);
                }
            }),
            Some(id),
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_ssh_row(
        &self,
        element_id: impl Into<gpui::ElementId>,
        title: impl Into<SharedString>,
        subtitle: impl Into<SharedString>,
        selected: bool,
        dot: Option<bool>,
        sf: presets::Surface,
        on_select: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static,
        menu_for: Option<Uuid>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let success = cx.theme().success;
        let border = cx.theme().border;
        let title: SharedString = title.into();
        let group_name = SharedString::from(format!("ssh-row-group-{title}"));
        let hover_group = group_name.clone();

        let row = h_flex()
            .id(element_id)
            .group(group_name)
            .items_center()
            .gap_2()
            .w_full()
            .py_2()
            .px_2()
            .rounded_md()
            .when(selected, |r| r.bg(gpui::rgb(sf.selected)))
            .when(!selected, |r| r.hover(|s| s.bg(gpui::rgb(sf.hover))))
            .on_mouse_down(MouseButton::Left, move |ev, window, cx| {
                cx.stop_propagation();
                on_select(ev, window, cx);
            })
            // The gutter is here on every row, dot or no dot. Only hosts carry
            // a liveness dot, and skipping the space on the rows that don't —
            // Defaults, and nothing else — started their title 14px left of
            // every host title under them.
            .child(
                div()
                    .flex_shrink_0()
                    .size(px(6.))
                    .when_some(dot, |d, live| {
                        d.rounded_full()
                            .when(live, |d| d.bg(success))
                            .when(!live, |d| d.border_1().border_color(border))
                    }),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_0p5()
                    .child(
                        div()
                            .text_sm()
                            .truncate()
                            .when(selected, |d| {
                                d.text_color(gpui::rgb(sf.text_selected))
                                    .font_weight(FontWeight::MEDIUM)
                            })
                            .when(!selected, |d| d.text_color(gpui::rgb(sf.text_resting)))
                            .child(title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .truncate()
                            .child(subtitle.into()),
                    ),
            );

        let Some(id) = menu_for else {
            return row.into_any_element();
        };
        let menu_app = cx.entity().downgrade();
        let ctx_app = cx.entity().downgrade();
        let row_idx = id.as_u128() as usize;
        row.child(
            div()
                .flex_shrink_0()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .when(!selected, move |s| {
                    s.opacity(0.).group_hover(hover_group, |s| s.opacity(1.))
                })
                .child(
                    Button::new(("ssh-prof-menu", row_idx))
                        .icon(Icon::empty().path("stock/icons/ellipsis.svg"))
                        .ghost()
                        .small()
                        .tooltip(t(L10nKey::TabTooltipMore))
                        .dropdown_menu_with_anchor(
                            gpui::Anchor::TopRight,
                            move |menu, _window, cx| {
                                Self::ssh_profile_row_menu(menu, id, cx.theme().danger, &menu_app)
                            },
                        ),
                ),
        )
        .context_menu(move |menu, _window, cx| {
            Self::ssh_profile_row_menu(menu, id, cx.theme().danger, &ctx_app)
        })
        .into_any_element()
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

    pub(crate) fn select_ssh_defaults(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.active_settings_mut() {
            s.ssh_form = None;
            s.ssh_detail = SshDetail::Defaults;
        }
        cx.notify();
    }

    fn toggle_ssh_group(&mut self, key: String, cx: &mut Context<Self>) {
        let selected_here = match self.active_settings().map(|s| s.ssh_detail) {
            Some(SshDetail::Profile(id)) => cx
                .global::<Config>()
                .ssh_profiles
                .iter()
                .find(|p| p.id == id)
                .is_some_and(|p| ssh_group_key(p) == key),
            _ => false,
        };
        let Some(s) = self.active_settings_mut() else {
            return;
        };
        let collapsing = !s.ssh_collapsed_groups.remove(&key);
        if collapsing {
            s.ssh_collapsed_groups.insert(key);
            if selected_here {
                s.ssh_form = None;
                s.ssh_detail = SshDetail::Defaults;
            }
        }
        cx.notify();
    }

    fn render_ssh_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let detail = self
            .active_settings()
            .map(|s| s.ssh_detail)
            .unwrap_or(SshDetail::None);
        match detail {
            SshDetail::Defaults => self.render_ssh_defaults_detail(cx),
            SshDetail::Profile(_)
                if self.active_settings().is_some_and(|s| s.ssh_form.is_some()) =>
            {
                self.render_ssh_profile_form(cx)
            }
            _ => self.render_ssh_empty_state(cx),
        }
    }

    fn render_ssh_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let Some(input) = self.active_settings().map(|s| s.ssh_quick_connect.clone()) else {
            return div().into_any_element();
        };
        let target = input.read(cx).value().trim().to_string();
        let parsed = crate::core::ssh_profile::parse_quick_connect(&target);
        let saved = cx.global::<Config>().ssh_profiles.len();

        let unlinked = {
            let known: std::collections::HashSet<String> = cx
                .global::<Config>()
                .ssh_profiles
                .iter()
                .map(|p| p.name.clone())
                .collect();
            crate::core::ssh_config::import_profiles()
                .into_iter()
                .filter(|i| !known.contains(&i.profile.name))
                .map(|i| i.profile.name)
                .collect::<Vec<_>>()
        };

        let heading = if saved == 0 {
            t(L10nKey::SettingsNoHostsYet)
        } else {
            t(L10nKey::SettingsNothingSelected)
        };

        let mut body = v_flex()
            .gap_1()
            .child(self.header_text(heading, cx))
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(t(L10nKey::SettingsTypeAddressToConnect)),
            )
            .child(
                h_flex()
                    .mt_3()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .max_w(px(320.))
                            .child(Input::new(&input).small()),
                    )
                    .child(
                        Button::new("ssh-quick-connect")
                            .label(t(L10nKey::Connect))
                            .primary()
                            .small()
                            .disabled(parsed.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.ssh_quick_connect_from_settings(window, cx)
                            })),
                    ),
            );

        if !unlinked.is_empty() {
            let n = unlinked.len();
            let names = unlinked.join(", ");
            body = body.child(
                h_flex()
                    .mt_6()
                    .gap_3()
                    .items_center()
                    .w_full()
                    .max_w(px(460.))
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(t_fmt(
                                L10nKey::SettingsMoreInSshConfig,
                                &[("count", &n.to_string())],
                            )))
                            .child(div().text_xs().text_color(muted).truncate().child(names)),
                    )
                    .child(
                        Button::new("ssh-empty-import")
                            .label(t(L10nKey::Link))
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.import_ssh_config_profiles(window, cx)
                            })),
                    ),
            );
        }

        body.into_any_element()
    }

    fn ssh_quick_connect_from_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self
            .active_settings()
            .map(|s| s.ssh_quick_connect.read(cx).value().trim().to_string())
        else {
            return;
        };
        let Some(qc) = crate::core::ssh_profile::parse_quick_connect(&target) else {
            return;
        };
        self.close_settings(window, cx);
        self.quick_connect(qc, window, cx);
    }

    fn render_ssh_defaults_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let imported = cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .filter(|p| p.group.as_deref() == Some(crate::core::ssh_config::IMPORTED_GROUP))
            .count();

        let config_block = v_flex()
            .child(self.section_intro(
                "~/.ssh/config",
                t_plural(L10nKey::SettingsAliasesLinked, imported, &[]),
                cx,
            ))
            .child(
                self.settings_row(
                    t(L10nKey::SettingsImportAliases),
                    t(L10nKey::SettingsImportAliasesDesc),
                    Button::new("ssh-defaults-import")
                        .label(t(L10nKey::SettingsImportNow))
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.import_ssh_config_profiles(window, cx)
                        }))
                        .into_any_element(),
                    cx,
                ),
            );

        v_flex()
            .child(
                v_flex()
                    .gap_1()
                    .mb_6()
                    .child(self.header_text(t(L10nKey::SettingsDefaults), cx))
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(t(L10nKey::SettingsDefaultsIntro)),
                    ),
            )
            .child(self.render_ssh_security_block(cx))
            .child(self.section_rule(cx))
            .child(config_block)
            .into_any_element()
    }

    fn ssh_profile_row_menu(
        menu: PopupMenu,
        id: Uuid,
        danger: gpui::Hsla,
        app: &gpui::WeakEntity<Self>,
    ) -> PopupMenu {
        let menu = menu
            .min_w(px(180.))
            .item(PopupMenuItem::new(t(L10nKey::Connect)).on_click({
                let app = app.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.close_settings(window, cx);
                        this.connect_ssh_profile(id, window, cx);
                    });
                }
            }))
            .item(
                PopupMenuItem::new(t(L10nKey::SettingsCopyAddress)).on_click({
                    let app = app.clone();
                    move |_, _window, cx| {
                        let _ = app.update(cx, |this, cx| this.copy_profile_connect_string(id, cx));
                    }
                }),
            )
            .item(PopupMenuItem::new(t(L10nKey::SettingsDuplicate)).on_click({
                let app = app.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| this.duplicate_profile(id, window, cx));
                }
            }))
            .item(
                PopupMenuItem::new(t(L10nKey::SettingsForgetPassword)).on_click({
                    let app = app.clone();
                    move |_, window, cx| {
                        let _ =
                            app.update(cx, |this, cx| this.forget_profile_password(id, window, cx));
                    }
                }),
            )
            .separator();

        menu.item(
            PopupMenuItem::element(move |_window, _cx| {
                div().text_color(danger).child(t(L10nKey::Delete))
            })
            .on_click({
                let app = app.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| this.delete_profile(id, window, cx));
                }
            }),
        )
    }

    fn render_ssh_security_block(&self, cx: &mut Context<Self>) -> AnyElement {
        let verify = cx.global::<Config>().verify_host_keys;
        let verify_switch = crate::ui::theme::switch("ssh-verify-host-keys", cx)
            .checked(verify)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_verify_host_keys(*on, cx)))
            .into_any_element();

        let warn_on_close = cx.global::<Config>().ssh_warn_on_close;
        let warn_switch = crate::ui::theme::switch("ssh-warn-on-close", cx)
            .checked(warn_on_close)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_ssh_warn_on_close(*on, cx)))
            .into_any_element();

        v_flex()
            .child(self.section_intro(
                t(L10nKey::SettingsSecurity),
                t(L10nKey::SettingsSecurityIntro),
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsVerifyHostKeys),
                t(L10nKey::SettingsVerifyHostKeysDesc),
                verify_switch,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::WarnBeforeClosing),
                t(L10nKey::SettingsWarnBeforeClosingDesc),
                warn_switch,
                cx,
            ))
            .into_any_element()
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

        let name = seed_input(window, cx, &profile.name, false);
        let host = seed_input(window, cx, &profile.host, false);
        let port = seed_input(window, cx, &profile.port.to_string(), false);
        let user = seed_input(window, cx, &profile.user, false);
        let jump = seed_input(window, cx, &jump_name, false);
        let forwards: Vec<ForwardRuleForm> = profile
            .forwards
            .iter()
            .map(|r| seed_forward_row(window, cx, r))
            .collect();
        let identity_files = seed_input(window, cx, &profile.identity_files.join("\n"), true);
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
        let mut watch = vec![
            &name,
            &host,
            &port,
            &user,
            &jump,
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
                cx.subscribe_in(input, window, |_this, _i, ev: &InputEvent, _w, cx| {
                    if matches!(ev, InputEvent::Change) {
                        cx.notify();
                    }
                }),
            );
        }

        let form = SshProfileForm {
            editing: profile.id,
            carry_group: profile.group.clone(),
            carry_credential_ref: profile.credential_ref.clone(),
            show_jump: profile.jump_host.is_some(),
            show_forwards: !profile.forwards.is_empty(),
            show_advanced: false,
            name,
            host,
            port,
            user,
            auth: profile.auth,
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
            verify_host_keys: profile.verify_host_keys,
            warn_on_close: profile.warn_on_close,
            _subs: subs,
        };
        let editing = form.editing;
        if let Some(s) = self.active_settings_mut() {
            s.ssh_form = Some(form);
            s.ssh_detail = SshDetail::Profile(editing);
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
            identity_files: raw(&form.identity_files),
            agent_forward: form.agent_forward,
            credential_ref: form.carry_credential_ref.clone(),
            forwards: form.forwards.iter().filter_map(|r| r.collect(cx)).collect(),
            keepalive_interval: val(&form.keepalive_interval),
            keepalive_count: val(&form.keepalive_count),
            connect_timeout: val(&form.connect_timeout),
            warn_on_close: form.warn_on_close,
            skip_banner: form.skip_banner,
            shell_integration: form.shell_integration,
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

    pub(crate) fn save_editing_profile(&mut self, cx: &mut Context<Self>) -> Option<Uuid> {
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
                *slot = profile;
            } else {
                cfg.ssh_profiles.push(profile);
            }
        });
        Some(id)
    }

    pub(crate) fn save_ssh_form(&mut self, cx: &mut Context<Self>) {
        self.save_editing_profile(cx);
        cx.notify();
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
        self.ssh_form_collect(cx).map(|(profile, _)| profile) != saved
    }

    /// Closing from Escape or the X is the user leaving; every other caller
    /// closes as the tail of something they explicitly chose, and has already
    /// saved or does not care.
    pub(crate) fn close_settings_checked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ssh_form_dirty(cx) {
            self.close_settings(window, cx);
            return;
        }
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            t(L10nKey::SettingsDiscardChangesTitle),
            Some(t(L10nKey::SettingsDiscardChangesBody)),
            &crate::ui::confirm_answers(t(L10nKey::EditorDiscard), t(L10nKey::SettingsKeepEditing)),
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(0) = answer.await else { return };
            let _ = this.update_in(cx, |this, window, cx| this.close_settings(window, cx));
        })
        .detach();
    }

    pub(crate) fn save_and_connect_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.save_editing_profile(cx) {
            self.close_settings(window, cx);
            self.connect_ssh_profile(id, window, cx);
        }
    }

    pub(crate) fn add_new_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profile = SshProfile::new(String::new());
        self.ssh_form_load(&profile, window, cx);
    }

    pub(crate) fn duplicate_profile(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut profile) = cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .find(|p| p.id == id)
            .cloned()
        else {
            return;
        };
        profile.id = Uuid::new_v4();
        profile.name = t_fmt(L10nKey::SettingsProfileCopied, &[("name", &profile.name)]);
        self.update_config(cx, |cfg| cfg.ssh_profiles.push(profile.clone()));
        self.ssh_form_load(&profile, window, cx);
    }

    pub(crate) fn delete_profile(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
        else {
            return;
        };
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &t_fmt(L10nKey::FileTreeDeleteTitle, &[("name", &name)]),
            Some(t(L10nKey::SettingsDeleteProfileBody)),
            &crate::ui::confirm_answers(t(L10nKey::Delete), t(L10nKey::Cancel)),
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(0) = answer.await else { return };
            let _ = this.update(cx, |this, cx| this.delete_profile_confirmed(id, cx));
        })
        .detach();
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
        self.update_config(cx, |cfg| {
            cfg.ssh_profiles.retain(|p| p.id != id);
            cfg.ssh_profile_frecency.remove(&id);
        });
        let editing_deleted =
            self.active_settings().map(|s| s.ssh_detail) == Some(SshDetail::Profile(id));
        if let Some(s) = self.active_settings_mut().filter(|_| editing_deleted) {
            s.ssh_form = None;
            s.ssh_detail = SshDetail::None;
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

    pub(crate) fn copy_profile_connect_string(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if let Some(profile) = cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .find(|p| p.id == id)
        {
            let s = to_connect_string(profile);
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(s));
        }
    }

    pub(crate) fn forget_profile_password(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cfg = cx.global::<Config>();
        let Some(endpoint) = cfg
            .ssh_profiles
            .iter()
            .find(|p| p.id == id)
            .map(|p| format!("{}@{}:{}", p.user, p.host, p.port))
        else {
            return;
        };
        // One click used to be the whole gesture, and the thing it removed does
        // not come back. Worse, the entry is the endpoint's rather than this
        // row's, so a menu opened on one host can sign several of them out —
        // name that count here instead of letting it turn up at the next
        // connect on a host nobody touched.
        let others = profiles_sharing_endpoint(cfg, id);
        let body = if others == 0 {
            t(L10nKey::SettingsForgetPasswordBody).to_string()
        } else {
            t_plural(
                L10nKey::SettingsForgetPasswordSharedBody,
                others,
                &[("endpoint", &endpoint)],
            )
        };
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &t_fmt(
                L10nKey::SettingsForgetPasswordTitle,
                &[("endpoint", &endpoint)],
            ),
            Some(&body),
            &crate::ui::confirm_answers(t(L10nKey::SettingsForgetPassword), t(L10nKey::Cancel)),
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(0) = answer.await else { return };
            // The notification is the only sign the keychain was touched, and
            // by now the click that asked for it is long gone — so it has to be
            // raised from in here, on the window the prompt belonged to.
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(msg) = this.forget_profile_password_confirmed(id, cx) {
                    window.push_notification(msg, cx);
                }
            });
        })
        .detach();
    }

    fn forget_profile_password_confirmed(
        &mut self,
        id: Uuid,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        use crate::core::keychain::{CredentialStore, OsCredentialStore};
        let (user, host, port) = cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .find(|p| p.id == id)
            .map(|p| (p.user.clone(), p.host.clone(), p.port))?;
        let endpoint = format!("{user}@{host}:{port}");
        Some(
            match OsCredentialStore.delete_password(&user, &host, port) {
                Ok(()) => t_fmt(
                    L10nKey::SettingsForgotPasswordFor,
                    &[("endpoint", &endpoint)],
                ),
                Err(e) => t_fmt(
                    L10nKey::SettingsCouldntForgetPassword,
                    &[("endpoint", &endpoint), ("error", &e.to_string())],
                ),
            },
        )
    }

    fn render_ssh_profile_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = self.active_settings().and_then(|s| s.ssh_form.as_ref()) else {
            return div().into_any_element();
        };
        let editing = form.editing;
        let muted = cx.theme().muted_foreground;
        let success = cx.theme().success;

        let saved = cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .find(|p| p.id == editing)
            .cloned();
        let (collected, errors) = self.ssh_form_collect(cx).unzip();
        let errors = errors.unwrap_or_default();
        let dirty = collected != saved;
        let address = collected
            .as_ref()
            .map(to_connect_string)
            .unwrap_or_default();
        let jump_name = form.jump.read(cx).value().trim().to_string();
        let live = self.live_ssh_profiles(cx).contains(&editing);
        let name = form.name.read(cx).value().trim().to_string();
        let host = form.host.read(cx).value().trim().to_string();
        let title = match (name.is_empty(), host.is_empty()) {
            (false, _) => name,
            (true, false) => host,
            (true, true) => t(L10nKey::SettingsNewHost).to_string(),
        };

        let auth_idx = match form.auth {
            AuthMode::Auto => 0,
            AuthMode::Gssapi => 1,
            AuthMode::Password => 2,
            AuthMode::PublicKey => 3,
            AuthMode::Agent => 4,
            AuthMode::KeyboardInteractive => 5,
        };
        let header = h_flex()
            .items_start()
            .justify_between()
            .gap_4()
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(title),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_xs()
                            .text_color(muted)
                            .child(div().truncate().child(address))
                            .when(!jump_name.is_empty(), |r| {
                                r.child(div().child(t_fmt(
                                    L10nKey::SettingsJumpHostVia,
                                    &[("jump_name", &jump_name)],
                                )))
                            })
                            .when(live, |r| {
                                r.child(
                                    div()
                                        .text_color(success)
                                        .child(format!("· {}", t(L10nKey::SettingsConnected))),
                                )
                            }),
                    ),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_2()
                    .child(
                        Button::new("ssh-form-save")
                            .label(t(L10nKey::Save))
                            .small()
                            .disabled(!dirty || !errors.is_empty())
                            .on_click(cx.listener(|this, _, _w, cx| this.save_ssh_form(cx))),
                    )
                    .child(
                        // Connect saves first, so it answers to the same
                        // rules. Before this it answered to none at all, and
                        // an empty host reached the socket layer as a DNS
                        // error about a name nobody typed.
                        Button::new("ssh-form-connect")
                            .label(t(L10nKey::Connect))
                            .primary()
                            .small()
                            .disabled(!errors.is_empty())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_and_connect_profile(window, cx)
                            })),
                    ),
            );

        // Every field notifies on change, so this form re-renders on each
        // keystroke: telling a brand-new host that it needs a host is
        // something it would say before the user had typed a character. Hold
        // that one line back until the group it belongs to has something in
        // it. A malformed value has nothing to wait for and says so at once.
        let core_blank = form.core_is_blank(cx);
        let host_error = errors
            .host
            .as_ref()
            .filter(|_| !core_blank)
            .map(|e| field_error(e.message(), cx));
        let port_error = errors.port.as_ref().map(|e| field_error(e.message(), cx));

        let core = v_flex()
            .gap_3()
            .child(
                self.settings_row(
                    t(L10nKey::SettingsName),
                    t(L10nKey::SettingsNameDesc),
                    div()
                        .w(px(260.))
                        .max_w_full()
                        .child(Input::new(&form.name).small())
                        .into_any_element(),
                    cx,
                ),
            )
            .child(
                self.settings_row(
                    t(L10nKey::SettingsHost),
                    t(L10nKey::SettingsHostDesc),
                    v_flex()
                        .gap_1()
                        .max_w_full()
                        .child(
                            h_flex()
                                .gap_2()
                                .max_w_full()
                                .child(
                                    div()
                                        .w(px(172.))
                                        .min_w_0()
                                        .child(Input::new(&form.host).small()),
                                )
                                .child(
                                    div()
                                        .w(px(80.))
                                        .flex_shrink_0()
                                        .child(Input::new(&form.port).small()),
                                ),
                        )
                        .when_some(host_error, |col, line| col.child(line))
                        .when_some(port_error, |col, line| col.child(line))
                        .into_any_element(),
                    cx,
                ),
            )
            .child(
                self.settings_row(
                    t(L10nKey::SettingsUser),
                    t(L10nKey::SettingsUserDesc),
                    div()
                        .w(px(260.))
                        .max_w_full()
                        .child(Input::new(&form.user).small())
                        .into_any_element(),
                    cx,
                ),
            )
            .child(self.settings_row(
                t(L10nKey::SettingsAuth),
                t(L10nKey::SettingsAuthDesc),
                self.segmented(
                    "ssh-form-auth",
                    &[
                        t(L10nKey::SettingsAuthModeAuto),
                        "GSSAPI",
                        t(L10nKey::SettingsAuthModePassword),
                        t(L10nKey::SettingsAuthModeKey),
                        t(L10nKey::SettingsAuthModeAgent),
                        t(L10nKey::SettingsAuthMode2Fa),
                    ],
                    auth_idx,
                    cx,
                    |this, ix, _w, cx| {
                        if let Some(f) = this.ssh_form_mut() {
                            f.auth = match ix {
                                0 => AuthMode::Auto,
                                1 => AuthMode::Gssapi,
                                2 => AuthMode::Password,
                                3 => AuthMode::PublicKey,
                                4 => AuthMode::Agent,
                                _ => AuthMode::KeyboardInteractive,
                            };
                            cx.notify();
                        }
                    },
                ),
                cx,
            ));

        v_flex()
            .gap_4()
            .child(header)
            .child(core)
            .child(self.render_ssh_profile_jump_section(form, &errors, cx))
            .child(self.render_ssh_profile_forwards_section(form, cx))
            .child(self.render_ssh_profile_advanced_section(form, &errors, cx))
            .into_any_element()
    }

    fn disclosure_header(
        &self,
        id: &'static str,
        label: &str,
        summary: &str,
        open: bool,
        cx: &mut Context<Self>,
        on_toggle: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let sf = cx.global::<presets::Surfaces>().window;
        let caret = if open { "▾" } else { "▸" };
        h_flex()
            .id(id)
            .items_center()
            .gap_2()
            .py_2()
            // The other collapsible header on this page lights up under the
            // pointer; this one only changed the cursor, so the two rows a
            // reader folds and unfolds answered differently to the same move.
            .px_2p5()
            .mx_neg_2p5()
            .rounded_lg()
            .cursor_pointer()
            .hover(|s| s.bg(gpui::rgb(sf.hover)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _w, cx| on_toggle(this, cx)),
            )
            .child(div().text_color(muted).child(caret.to_string()))
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(label.to_string()),
            )
            .child(div().text_xs().text_color(muted).child(summary.to_string()))
            .into_any_element()
    }

    fn render_ssh_profile_jump_section(
        &self,
        form: &SshProfileForm,
        errors: &SshFormErrors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let summary = {
            let name = form.jump.read(cx).value().trim().to_string();
            if name.is_empty() {
                t(L10nKey::SettingsNoneSummary).to_string()
            } else {
                name
            }
        };
        // A complaint nobody can see is a Save button that is greyed out for
        // no reason the user can read, and the field keeps its text whether
        // this section is folded or not — so an error holds it open.
        let open = form.show_jump || errors.jump.is_some();
        let mut section = v_flex().child(self.disclosure_header(
            "ssh-sec-jump",
            t(L10nKey::SettingsJumpHost),
            &summary,
            open,
            cx,
            |this, cx| {
                if let Some(f) = this.ssh_form_mut() {
                    f.show_jump = !f.show_jump;
                    cx.notify();
                }
            },
        ));
        if open {
            let error = errors.jump.as_ref().map(|e| field_error(e.message(), cx));
            section = section.child(
                self.settings_row(
                    t(L10nKey::SettingsJumpHost),
                    t(L10nKey::SettingsJumpHostDesc),
                    v_flex()
                        .gap_1()
                        .w(px(260.))
                        .max_w_full()
                        .child(Input::new(&form.jump).small())
                        .when_some(error, |col, line| col.child(line))
                        .into_any_element(),
                    cx,
                ),
            );
        }
        section.into_any_element()
    }

    fn render_ssh_profile_forwards_section(
        &self,
        form: &SshProfileForm,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let count = form
            .forwards
            .iter()
            .filter(|r| r.collect(cx).is_some())
            .count();
        let summary = match count {
            0 => t(L10nKey::SettingsNoneSummary).to_string(),
            _ => t_plural(L10nKey::SettingsRulesOpenedWithConnection, count, &[]),
        };
        let mut section = v_flex().child(self.disclosure_header(
            "ssh-sec-fwd",
            t(L10nKey::SettingsPortForwarding),
            &summary,
            form.show_forwards,
            cx,
            |this, cx| {
                if let Some(f) = this.ssh_form_mut() {
                    f.show_forwards = !f.show_forwards;
                    cx.notify();
                }
            },
        ));
        if !form.show_forwards {
            return section.into_any_element();
        }

        for (idx, row) in form.forwards.iter().enumerate() {
            section = section.child(self.render_forward_rule_row(idx, row, cx));
        }

        section
            .child(
                h_flex().pt_1p5().child(
                    Button::new("ssh-fwd-add")
                        .label(t(L10nKey::SettingsAddRule))
                        .ghost()
                        .small()
                        .on_click(
                            cx.listener(|this, _, window, cx| this.add_forward_rule(window, cx)),
                        ),
                ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_3()
                    .pt_1()
                    .text_xs()
                    .text_color(muted)
                    .child(t(L10nKey::SettingsFwdLegendLocal))
                    .child(t(L10nKey::SettingsFwdLegendRemote))
                    .child(t(L10nKey::SettingsFwdLegendDynamic)),
            )
            .into_any_element()
    }

    fn render_forward_rule_row(
        &self,
        idx: usize,
        row: &ForwardRuleForm,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let needs_target = row.kind != ForwardKind::Dynamic;
        let kind_idx = match row.kind {
            ForwardKind::Local => 0,
            ForwardKind::Remote => 1,
            ForwardKind::Dynamic => 2,
        };
        let incomplete = (row.collect(cx).is_none() && !row.is_blank(cx)).then(|| {
            field_error(
                match needs_target {
                    true => t(L10nKey::SettingsFwdNeedsBoth),
                    false => t(L10nKey::SettingsFwdNeedsListen),
                },
                cx,
            )
        });

        // Below `SPLIT_FORWARD_ROW_BELOW` the five controls stop fitting on one
        // line. The kind switch, the description and the remove button keep the
        // first line; the mapping the rule is actually about takes the second,
        // where two host fields, two ports and an arrow are what has to fit
        // inside `CONTENT_MIN_W` — hence the lower floor on the host field.
        let split = self.settings_row_under(SPLIT_FORWARD_ROW_BELOW, cx);
        let stack_ends = self.settings_row_under(STACK_FORWARD_ENDS_BELOW, cx);
        let host_min = if split { 80. } else { 104. };
        let endpoint = |host: &Entity<InputState>, port: &Entity<InputState>| {
            h_flex()
                .gap_1()
                .items_center()
                // Was pinned at 104px, which does not hold
                // `ip-10-0-3-217.eu-west-1.compute.internal`. Same floor, but
                // the field now takes a share of the row's slack instead of
                // handing all of it to the free-text description beside it.
                .child(
                    div()
                        .flex_1()
                        .min_w(px(host_min))
                        .child(Input::new(host).xsmall()),
                )
                .child(div().text_xs().text_color(muted).child(":"))
                .child(div().w(px(58.)).child(Input::new(port).xsmall()))
        };
        let mapping = |line: Div| {
            line.child(
                div()
                    .flex_1()
                    .when(stack_ends, |end| end.w_full())
                    .child(endpoint(&row.bind_host, &row.bind_port)),
            )
            .child(div().flex_shrink_0().text_xs().text_color(muted).child("→"))
            .child(
                div()
                    .flex_1()
                    .opacity(if needs_target {
                        1.0
                    } else {
                        crate::ui::forwards::NO_TARGET_FADE
                    })
                    .when(stack_ends, |end| end.w_full())
                    .child(endpoint(&row.target_host, &row.target_port)),
            )
        };

        let kind_switch = div().flex_shrink_0().child(self.segmented(
            format!("ssh-fwd-kind-{idx}"),
            &["L", "R", "D"],
            kind_idx,
            cx,
            move |this, ix, _w, cx| {
                let kind = match ix {
                    1 => ForwardKind::Remote,
                    2 => ForwardKind::Dynamic,
                    _ => ForwardKind::Local,
                };
                if let Some(f) = this.ssh_form_mut()
                    && let Some(r) = f.forwards.get_mut(idx)
                {
                    r.kind = kind;
                    cx.notify();
                }
            },
        ));
        let description = div()
            .flex_1()
            .min_w(px(80.))
            .child(Input::new(&row.description).xsmall());
        let remove = crate::ui::tab_strip::hit_target(
            Button::new(("ssh-fwd-remove", idx))
                .icon(Icon::new(IconName::Close))
                .ghost()
                .xsmall(),
        )
        .tooltip(t(L10nKey::SettingsRemoveRule))
        .on_click(cx.listener(move |this, _, _w, cx| this.remove_forward_rule(idx, cx)));

        let rule = match split {
            true => v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(kind_switch)
                        .child(description)
                        .child(remove),
                )
                .child(match stack_ends {
                    true => mapping(v_flex().gap_1().items_start()),
                    false => mapping(h_flex().gap_2().items_center()),
                }),
            false => mapping(h_flex().gap_2().items_center().child(kind_switch))
                .child(description)
                .child(remove),
        };

        v_flex()
            .gap_0p5()
            .py_1()
            .child(rule)
            .when_some(incomplete, |col, line| col.child(line))
            .into_any_element()
    }

    fn add_forward_rule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = seed_forward_row(window, cx, &ForwardRule::default());
        let subs: Vec<_> = forward_row_inputs(&row)
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |_this, _i, ev: &InputEvent, _w, cx| {
                    if matches!(ev, InputEvent::Change) {
                        cx.notify();
                    }
                })
            })
            .collect();
        if let Some(f) = self.ssh_form_mut() {
            f.forwards.push(row);
            f._subs.extend(subs);
            f.show_forwards = true;
        }
        cx.notify();
    }

    fn remove_forward_rule(&mut self, idx: usize, cx: &mut Context<Self>) {
        if let Some(f) = self.ssh_form_mut()
            && idx < f.forwards.len()
        {
            f.forwards.remove(idx);
        }
        cx.notify();
    }

    fn render_ssh_profile_advanced_section(
        &self,
        form: &SshProfileForm,
        errors: &SshFormErrors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // This section opens folded, and a proxy address saved back when the
        // form wrote port 0 is wrong the moment the profile is opened. Let the
        // error unfold it, or Save is disabled over something out of sight.
        let open = form.show_advanced || errors.socks.is_some() || errors.http.is_some();
        let mut section = v_flex().child(self.disclosure_header(
            "ssh-sec-adv",
            t(L10nKey::SettingsAdvanced),
            t(L10nKey::SettingsAdvancedSummary),
            open,
            cx,
            |this, cx| {
                if let Some(f) = this.ssh_form_mut() {
                    f.show_advanced = !f.show_advanced;
                    cx.notify();
                }
            },
        ));
        if !open {
            return section.into_any_element();
        }

        let text_row = |this: &Self,
                        label: &str,
                        desc: &str,
                        input: &Entity<InputState>,
                        cx: &mut Context<Self>| {
            this.settings_row(
                label.to_string(),
                desc.to_string(),
                div()
                    .w(px(260.))
                    .max_w_full()
                    .child(Input::new(input).small())
                    .into_any_element(),
                cx,
            )
        };
        // The two proxy addresses are the only advanced fields with a rule of
        // their own, so they carry room for the complaint under the control.
        let proxy_row = |this: &Self,
                         label: &str,
                         desc: &str,
                         input: &Entity<InputState>,
                         error: Option<&SshFieldError>,
                         cx: &mut Context<Self>| {
            let line = error.map(|e| field_error(e.message(), cx));
            this.settings_row(
                label.to_string(),
                desc.to_string(),
                v_flex()
                    .gap_1()
                    .w(px(260.))
                    .max_w_full()
                    .child(Input::new(input).small())
                    .when_some(line, |col, line| col.child(line))
                    .into_any_element(),
                cx,
            )
        };

        let on_off = |b: bool| {
            if b {
                t(L10nKey::SettingsValueOn)
            } else {
                t(L10nKey::SettingsValueOff)
            }
        };
        let vhk_default = on_off(cx.global::<Config>().verify_host_keys);
        let woc_default = on_off(cx.global::<Config>().ssh_warn_on_close);
        let vhk_idx = match form.verify_host_keys {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };
        let woc_idx = match form.warn_on_close {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };

        section = section
            .child(self.subgroup_header(L10nKey::SettingsGroupAuthentication, cx))
            .child(text_row(
                self,
                t(L10nKey::SettingsIdentityFiles),
                t(L10nKey::SettingsIdentityFilesDesc),
                &form.identity_files,
                cx,
            ))
            .child(
                self.settings_row(
                    t(L10nKey::SettingsAgentForwarding),
                    t(L10nKey::SettingsAgentForwardingDesc),
                    crate::ui::theme::switch("ssh-form-agent", cx)
                        .checked(form.agent_forward)
                        .on_click(cx.listener(|this, on: &bool, _w, cx| {
                            if let Some(f) = this.ssh_form_mut() {
                                f.agent_forward = *on;
                                cx.notify();
                            }
                        }))
                        .into_any_element(),
                    cx,
                ),
            )
            .child(self.subgroup_header(L10nKey::SettingsGroupProxies, cx))
            .child(text_row(
                self,
                t(L10nKey::SettingsProxyCommand),
                t(L10nKey::SettingsProxyCommandDesc),
                &form.proxy_command,
                cx,
            ))
            .child(proxy_row(
                self,
                t(L10nKey::SettingsSocks5Proxy),
                t(L10nKey::SettingsSocks5ProxyDesc),
                &form.socks,
                errors.socks.as_ref(),
                cx,
            ))
            .child(proxy_row(
                self,
                t(L10nKey::SettingsHttpProxy),
                t(L10nKey::SettingsHttpProxyDesc),
                &form.http,
                errors.http.as_ref(),
                cx,
            ))
            .child(self.subgroup_header(L10nKey::SettingsGroupAlgorithms, cx))
            .child(text_row(
                self,
                t(L10nKey::SettingsKexAlgorithms),
                t(L10nKey::SettingsKexAlgorithmsDesc),
                &form.kex,
                cx,
            ))
            .child(text_row(
                self,
                t(L10nKey::SettingsCiphers),
                t(L10nKey::SettingsCiphersDesc),
                &form.cipher,
                cx,
            ))
            .child(text_row(
                self,
                t(L10nKey::SettingsMacs),
                t(L10nKey::SettingsMacsDesc),
                &form.mac,
                cx,
            ))
            .child(text_row(
                self,
                t(L10nKey::SettingsHostKeyAlgorithms),
                t(L10nKey::SettingsHostKeyAlgorithmsDesc),
                &form.hostkey,
                cx,
            ))
            // Compression here is the algorithm list russh negotiates, not
            // ssh_config's yes/no switch, so it belongs with the other three
            // lists rather than under Connection with the keepalives.
            .child(text_row(
                self,
                t(L10nKey::SettingsCompression),
                t(L10nKey::SettingsCompressionDesc),
                &form.compression,
                cx,
            ))
            .child(self.subgroup_header(L10nKey::SettingsGroupConnection, cx))
            .child(text_row(
                self,
                t(L10nKey::SettingsKeepaliveInterval),
                t(L10nKey::SettingsKeepaliveIntervalDesc),
                &form.keepalive_interval,
                cx,
            ))
            .child(text_row(
                self,
                t(L10nKey::SettingsKeepaliveCountMax),
                t(L10nKey::SettingsKeepaliveCountMaxDesc),
                &form.keepalive_count,
                cx,
            ))
            .child(text_row(
                self,
                t(L10nKey::SettingsConnectTimeout),
                t(L10nKey::SettingsConnectTimeoutDesc),
                &form.connect_timeout,
                cx,
            ))
            .child(self.subgroup_header(L10nKey::SettingsGroupSession, cx))
            .child(
                self.settings_row(
                    t(L10nKey::SettingsX11Forwarding),
                    t(L10nKey::SettingsX11ForwardingDesc),
                    crate::ui::theme::switch("ssh-form-x11", cx)
                        .checked(form.x11)
                        .on_click(cx.listener(|this, on: &bool, _w, cx| {
                            if let Some(f) = this.ssh_form_mut() {
                                f.x11 = *on;
                                cx.notify();
                            }
                        }))
                        .into_any_element(),
                    cx,
                ),
            )
            .child(
                self.settings_row(
                    t(L10nKey::SettingsShellIntegration),
                    t(L10nKey::SettingsShellIntegrationDesc),
                    crate::ui::theme::switch("ssh-form-shell-integration", cx)
                        .checked(form.shell_integration)
                        .on_click(cx.listener(|this, on: &bool, _w, cx| {
                            if let Some(f) = this.ssh_form_mut() {
                                f.shell_integration = *on;
                                cx.notify();
                            }
                        }))
                        .into_any_element(),
                    cx,
                ),
            )
            .child(text_row(
                self,
                t(L10nKey::SettingsLoginScripts),
                t(L10nKey::SettingsLoginScriptsDesc),
                &form.login_scripts,
                cx,
            ))
            .child(
                self.settings_row(
                    t(L10nKey::SettingsSkipBanner),
                    t(L10nKey::SettingsSkipBannerDesc),
                    crate::ui::theme::switch("ssh-form-banner", cx)
                        .checked(form.skip_banner)
                        .on_click(cx.listener(|this, on: &bool, _w, cx| {
                            if let Some(f) = this.ssh_form_mut() {
                                f.skip_banner = *on;
                                cx.notify();
                            }
                        }))
                        .into_any_element(),
                    cx,
                ),
            )
            .child(self.subgroup_header(L10nKey::SettingsGroupSecurity, cx))
            .child(self.settings_row(
                t(L10nKey::SettingsVerifyHostKeys),
                t_fmt(
                    L10nKey::SettingsDefaultFollowsDefaults,
                    &[("value", vhk_default)],
                ),
                self.segmented(
                    "ssh-form-vhk",
                    &[
                        t(L10nKey::SettingsDefault),
                        t(L10nKey::SettingsOn),
                        t(L10nKey::SettingsOff),
                    ],
                    vhk_idx,
                    cx,
                    |this, ix, _w, cx| {
                        if let Some(f) = this.ssh_form_mut() {
                            f.verify_host_keys = match ix {
                                1 => Some(true),
                                2 => Some(false),
                                _ => None,
                            };
                            cx.notify();
                        }
                    },
                ),
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::WarnBeforeClosing),
                t_fmt(
                    L10nKey::SettingsDefaultFollowsDefaults,
                    &[("value", woc_default)],
                ),
                self.segmented(
                    "ssh-form-woc",
                    &[
                        t(L10nKey::SettingsDefault),
                        t(L10nKey::SettingsOn),
                        t(L10nKey::SettingsOff),
                    ],
                    woc_idx,
                    cx,
                    |this, ix, _w, cx| {
                        if let Some(f) = this.ssh_form_mut() {
                            f.warn_on_close = match ix {
                                1 => Some(true),
                                2 => Some(false),
                                _ => None,
                            };
                            cx.notify();
                        }
                    },
                ),
                cx,
            ));
        section.into_any_element()
    }

    fn render_shell_group(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted_fg = cx.theme().muted_foreground;
        let (program_input, args_input, wd_path_input) = match self.active_settings() {
            Some(s) => (
                s.shell_program_input.clone(),
                s.shell_args_input.clone(),
                s.wd_path_input.clone(),
            ),
            None => return div().into_any_element(),
        };
        let wd_strategy = cx.global::<Config>().working_directory.strategy;

        let platform_default = if cfg!(windows) {
            "PowerShell"
        } else {
            t(L10nKey::SettingsShellDefaultLoginShell)
        };

        // tty7 already knows which shells are installed — it lists them on the
        // new-tab button. Settings asked you to type one from memory instead,
        // so the same choice was a menu in one place and a blind text field in
        // the other. The field stays: a shell tty7 did not find still has to be
        // reachable by path.
        let shells = self.shells.shells.clone();
        let current_program = program_input.read(cx).value().trim().to_string();
        let platform_default_item: SharedString = if cfg!(windows) {
            "PowerShell".into()
        } else {
            t(L10nKey::AppPlaceholderLoginShell).into()
        };
        let picker_app = cx.entity().downgrade();
        let picker_input = program_input.clone();
        // The chevron rides inside the field rather than beside it: hung on the
        // outside it would either push the box narrower than the Arguments box
        // directly below or push past the column every other control ends on.
        let program_picker = crate::ui::tab_strip::hit_target(
            Button::new("shell-program-detected")
                .icon(IconName::ChevronDown)
                .ghost()
                .xsmall(),
        )
        .disabled(shells.is_empty())
        .tooltip(t(L10nKey::SettingsShellDetected))
        .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _window, _cx| {
            let mut menu = menu.min_w(px(200.));
            let pick = |program: String| {
                let app = picker_app.clone();
                let input = picker_input.clone();
                move |_: &_, window: &mut Window, cx: &mut App| {
                    input.update(cx, |state, cx| state.set_value(program.clone(), window, cx));
                    if let Some(app) = app.upgrade() {
                        app.update(cx, |this, cx| this.commit_shell_from_picker(cx));
                    }
                }
            };
            menu = menu.item(
                PopupMenuItem::new(platform_default_item.clone())
                    .checked(current_program.is_empty())
                    .on_click(pick(String::new())),
            );
            if !shells.is_empty() {
                menu = menu.item(PopupMenuItem::separator());
            }
            for shell in &shells {
                menu = menu.item(
                    PopupMenuItem::new(shell.label.clone())
                        .checked(current_program == shell.program)
                        .on_click(pick(shell.program.clone())),
                );
            }
            menu
        });
        let program_control = div()
            .w(px(260.))
            .max_w_full()
            .child(Input::new(&program_input).small().suffix(program_picker))
            .into_any_element();
        let args_control = div()
            .w(px(260.))
            .max_w_full()
            .child(Input::new(&args_input).small())
            .into_any_element();

        use crate::core::config::WdStrategy;
        let wd_idx = match wd_strategy {
            WdStrategy::Inherit => 0,
            WdStrategy::Home => 1,
            WdStrategy::Custom => 2,
        };
        let wd_radio = self.segmented(
            "wd-strategy",
            &[
                t(L10nKey::SettingsWdInherit),
                t(L10nKey::SettingsWdHome),
                t(L10nKey::SettingsWdCustom),
            ],
            wd_idx,
            cx,
            |this, ix, _w, cx| {
                let s = match ix {
                    0 => WdStrategy::Inherit,
                    1 => WdStrategy::Home,
                    _ => WdStrategy::Custom,
                };
                this.set_working_directory_strategy(s, cx);
            },
        );
        let wd_path_control = if wd_strategy == WdStrategy::Custom {
            div()
                .w(px(260.))
                .max_w_full()
                .child(Input::new(&wd_path_input).small())
                .into_any_element()
        } else {
            div().into_any_element()
        };

        v_flex()
            .child(self.section_intro(
                t(L10nKey::SettingsShell),
                t_fmt(
                    L10nKey::SettingsShellIntro,
                    &[("default", platform_default)],
                ),
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsProgram),
                t(L10nKey::SettingsProgramDesc),
                program_control,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsArguments),
                t(L10nKey::SettingsArgumentsDesc),
                args_control,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsStartIn),
                t(L10nKey::SettingsStartInDesc),
                wd_radio,
                cx,
            ))
            .when(
                wd_strategy == crate::core::config::WdStrategy::Custom,
                |v| {
                    v.child(self.settings_row(
                        t(L10nKey::SettingsCustomPath),
                        t(L10nKey::SettingsCustomPathDesc),
                        wd_path_control,
                        cx,
                    ))
                },
            )
            .child(
                div()
                    .mt_3()
                    .text_xs()
                    .text_color(muted_fg)
                    .child(t(L10nKey::SettingsShellFooter)),
            )
            .into_any_element()
    }

    fn render_settings_terminal(&self, cx: &mut Context<Self>) -> AnyElement {
        let foreground = cx.theme().foreground;
        let cfg = cx.global::<Config>();
        let link_url = cfg.link_url;
        let ssh_loopback_forward = cfg.ssh_loopback_forward;
        let mouse_hide = cfg.mouse_hide_while_typing;
        let focus_follows = cfg.focus_follows_mouse;
        let scroll_mult = cfg.mouse_scroll_multiplier;
        let smooth_scroll = cfg.smooth_scroll;
        let mouse_reporting = cfg.mouse_reporting;
        let bell = cfg.bell;
        let scrollback_idx = match cfg.scrollback_limit {
            n if n <= 1_000 => 0,
            n if n <= 10_000 => 1,
            _ => 2,
        };
        let scroll_slider = match self.active_settings() {
            Some(s) => s.scroll_slider.clone(),
            None => return div().into_any_element(),
        };
        let link_file_command_input = match self.active_settings() {
            Some(s) => s.link_file_command_input.clone(),
            None => return div().into_any_element(),
        };

        let link_switch = crate::ui::theme::switch("term-link-url", cx)
            .checked(link_url)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_link_url(*on, cx)))
            .into_any_element();
        let ssh_loopback_switch = crate::ui::theme::switch("term-ssh-loopback-forward", cx)
            .checked(ssh_loopback_forward)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_ssh_loopback_forward(*on, cx)))
            .into_any_element();
        let link_file_command_control = div()
            .w(px(300.))
            .max_w_full()
            .child(Input::new(&link_file_command_input).small())
            .into_any_element();
        let scrollback_radio = self.segmented(
            "term-scrollback",
            &["1,000", "10,000", "100,000"],
            scrollback_idx,
            cx,
            |this, ix, _w, cx| {
                let lines = match ix {
                    0 => 1_000,
                    1 => 10_000,
                    _ => 100_000,
                };
                this.set_scrollback_limit(lines, cx);
            },
        );

        let focus_switch = crate::ui::theme::switch("term-focus-follows", cx)
            .checked(focus_follows)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_focus_follows_mouse(*on, cx)))
            .into_any_element();
        let mouse_hide_switch = crate::ui::theme::switch("term-mouse-hide", cx)
            .checked(mouse_hide)
            .on_click(
                cx.listener(|this, on: &bool, _w, cx| this.set_mouse_hide_while_typing(*on, cx)),
            )
            .into_any_element();
        let mouse_report_switch = crate::ui::theme::switch("term-mouse-report", cx)
            .checked(mouse_reporting)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_mouse_reporting(*on, cx)))
            .into_any_element();
        let bell_idx = match bell {
            BellMode::None => 0,
            BellMode::Visual => 1,
            BellMode::Audible => 2,
            BellMode::Both => 3,
        };
        let bell_control = self.segmented(
            "term-bell",
            &[
                t(L10nKey::SettingsBellModeOff),
                t(L10nKey::SettingsBellModeVisual),
                t(L10nKey::SettingsBellModeAudible),
                t(L10nKey::SettingsBellModeBoth),
            ],
            bell_idx,
            cx,
            |this, ix, _w, cx| {
                let mode = match ix {
                    0 => BellMode::None,
                    1 => BellMode::Visual,
                    2 => BellMode::Audible,
                    3 => BellMode::Both,
                    _ => BellMode::default(),
                };
                this.set_bell_mode(mode, cx);
            },
        );
        let scroll_control = h_flex()
            .items_center()
            .gap_3()
            .w(px(240.))
            .max_w_full()
            .child(div().flex_1().child(Slider::new(&scroll_slider)))
            .child(
                div()
                    .w(px(38.))
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_right()
                    .text_sm()
                    .text_color(foreground)
                    .child(format!("{scroll_mult:.2}×")),
            )
            .into_any_element();
        let smooth_scroll_switch = crate::ui::theme::switch("term-smooth-scroll", cx)
            .checked(smooth_scroll)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_smooth_scroll(*on, cx)))
            .into_any_element();

        v_flex()
            .child(self.render_shell_group(cx))
            .child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsScrolling), cx))
            .child(self.settings_row(
                t(L10nKey::SettingsScrollback),
                t(L10nKey::SettingsScrollbackDesc),
                scrollback_radio,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsScrollSpeed),
                t(L10nKey::SettingsScrollSpeedDesc),
                scroll_control,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsSmoothScroll),
                t(L10nKey::SettingsSmoothScrollDesc),
                smooth_scroll_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsMouse), cx))
            .child(self.settings_row(
                t(L10nKey::SettingsFocusFollowsMouse),
                t(L10nKey::SettingsFocusFollowsMouseDesc),
                focus_switch,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsHideMouseWhileTyping),
                t(L10nKey::SettingsHideMouseWhileTypingDesc),
                mouse_hide_switch,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsReportMouseToApps),
                t(L10nKey::SettingsReportMouseToAppsDesc),
                mouse_report_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsBell), cx))
            .child(self.settings_row(
                t(L10nKey::SettingsTerminalBell),
                t(L10nKey::SettingsTerminalBellDesc),
                bell_control,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsLinks), cx))
            .child(self.settings_row(
                t(L10nKey::DetectUrls),
                t_fmt(
                    L10nKey::SettingsDetectUrlsDesc,
                    &[("modifier", LINK_MODIFIER_LABEL)],
                ),
                link_switch,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::ForwardSshLoopbackLinks),
                t(L10nKey::SettingsForwardSshLoopbackLinksDesc),
                ssh_loopback_switch,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::OpenFilesWith),
                t_fmt(
                    L10nKey::SettingsOpenFilesWithDesc,
                    &[
                        ("modifier", LINK_MODIFIER_LABEL),
                        ("path", "{path}"),
                        ("line", "{line}"),
                        ("column", "{column}"),
                    ],
                ),
                link_file_command_control,
                cx,
            ))
            .into_any_element()
    }

    fn render_settings_input(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg = cx.global::<Config>();
        let option_as_alt = cfg.macos_option_as_alt;
        let tab_completion = cfg.tab_completion;
        let history_search = cfg.history_search;
        let per_pane_history = cfg.per_pane_history;
        let smart_select = cfg.smart_select;
        let copy_on_select = cfg.copy_on_select;
        let clip_trim = cfg.clipboard_trim_trailing_spaces;

        let tab_completion_switch = crate::ui::theme::switch("term-tab-completion", cx)
            .checked(tab_completion)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_tab_completion(*on, cx)))
            .into_any_element();
        let history_search_switch = crate::ui::theme::switch("term-history-search", cx)
            .checked(history_search)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_history_search(*on, cx)))
            .into_any_element();
        let per_pane_history_switch = crate::ui::theme::switch("term-per-pane-history", cx)
            .checked(per_pane_history)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_per_pane_history(*on, cx)))
            .into_any_element();
        let smart_select_switch = crate::ui::theme::switch("term-smart-select", cx)
            .checked(smart_select)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_smart_select(*on, cx)))
            .into_any_element();
        let copy_on_select_switch = crate::ui::theme::switch("term-copy-on-select", cx)
            .checked(copy_on_select)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_copy_on_select(*on, cx)))
            .into_any_element();
        let trim_switch = crate::ui::theme::switch("term-clip-trim", cx)
            .checked(clip_trim)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_clipboard_trim(*on, cx)))
            .into_any_element();
        let option_alt_row = cfg!(target_os = "macos").then(|| {
            let switch = crate::ui::theme::switch("term-option-as-alt", cx)
                .checked(option_as_alt)
                .on_click(
                    cx.listener(|this, on: &bool, _w, cx| this.set_macos_option_as_alt(*on, cx)),
                )
                .into_any_element();
            self.settings_row(
                t(L10nKey::SettingsOptionAsMeta),
                t(L10nKey::SettingsOptionAsMetaDesc),
                switch,
                cx,
            )
        });

        v_flex()
            .child(self.section_intro(
                t(L10nKey::SettingsPrompt),
                t(L10nKey::SettingsPromptIntro),
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsTabCompletion),
                t(L10nKey::SettingsTabCompletionDesc),
                tab_completion_switch,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsHistorySearch),
                t(L10nKey::SettingsHistorySearchDesc),
                history_search_switch,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsPerPaneHistory),
                t(L10nKey::SettingsPerPaneHistoryDescription),
                per_pane_history_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsSelectionClipboard), cx))
            .child(self.settings_row(
                t(L10nKey::SettingsSmartSelection),
                t(L10nKey::SettingsSmartSelectionDesc),
                smart_select_switch,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsCopyOnSelect),
                t(L10nKey::SettingsCopyOnSelectDesc),
                copy_on_select_switch,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsTrimTrailingSpaces),
                t(L10nKey::SettingsTrimTrailingSpacesDesc),
                trim_switch,
                cx,
            ))
            .when_some(option_alt_row, |v, row| {
                v.child(self.section_rule(cx))
                    .child(self.section_header(t(L10nKey::SettingsKeyboard), cx))
                    .child(row)
            })
            .into_any_element()
    }

    fn render_settings_agents(&self, cx: &mut Context<Self>) -> AnyElement {
        use crate::core::agent_hooks::HooksState;

        let theme = cx.theme();
        let (foreground, muted_fg) = (theme.foreground, theme.muted_foreground);
        let (success, warning) = (theme.success, theme.warning);
        let (view, note, selected_host) = match self.active_settings() {
            Some(s) => (
                s.agent_hooks_states.clone(),
                s.agent_hooks_note.clone(),
                s.agent_hooks_host,
            ),
            None => (AgentHooksView::Loading, None, HostId::LOCAL),
        };
        let stacked = self.settings_row_under(STACK_ROW_BELOW, cx);
        let mut page = v_flex().child(self.section_intro(
            t(L10nKey::SettingsAgentsIntro),
            t(L10nKey::SettingsAgentsIntroDesc),
            cx,
        ));

        page = page.children(self.agent_hooks_machine_picker(selected_host, cx));

        // The hook rows describe whichever machine is selected above; the
        // command-line section below is always about this GUI's own host, so it
        // is appended after the match rather than inside the ready arm.
        match view {
            AgentHooksView::Loading => {
                page = page.child(
                    div()
                        .py_4()
                        .text_sm()
                        .text_color(muted_fg)
                        .child(t(L10nKey::SettingsReadingAgentConfig)),
                );
            }
            AgentHooksView::Unavailable(reason) => {
                page = page.child(div().py_4().text_sm().text_color(warning).child(reason));
            }
            AgentHooksView::Ready(rows) => {
                for (i, row) in rows.into_iter().enumerate() {
                    let agent = row.agent;
                    let (dot_color, status_text) = match row.state {
                        HooksState::NotInstalled => {
                            (muted_fg, t(L10nKey::SettingsStatusNotInstalled))
                        }
                        HooksState::Installed => (success, t(L10nKey::SettingsStatusInstalled)),
                        HooksState::Outdated => (warning, t(L10nKey::SettingsStatusOutdated)),
                    };
                    let primary_label = match row.state {
                        HooksState::NotInstalled => t(L10nKey::SettingsInstall),
                        HooksState::Installed => t(L10nKey::SettingsReinstall),
                        HooksState::Outdated => t(L10nKey::SettingsUpdate),
                    };
                    let row_note = note
                        .as_ref()
                        .filter(|(for_agent, _)| *for_agent == agent)
                        .map(|(_, text)| text.clone());

                    // Right-aligned beside its label, left-aligned under it —
                    // `settings_row` gives the control column the whole row
                    // once it stacks, and buttons flush to the far edge of a
                    // row whose label starts at the near one read as unrelated.
                    let control = v_flex()
                        .gap_2()
                        .when(!stacked, |c| c.items_end())
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(div().size_2().rounded_full().bg(dot_color))
                                .child(div().text_sm().text_color(foreground).child(status_text)),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new(("agent-hooks-install", i))
                                        .label(primary_label)
                                        .small()
                                        .on_click(cx.listener(move |this, _, _w, cx| {
                                            this.settings_install_agent_hooks(agent, cx)
                                        })),
                                )
                                .when(row.state != HooksState::NotInstalled, |r| {
                                    r.child(
                                        Button::new(("agent-hooks-uninstall", i))
                                            .label(t(L10nKey::SettingsUninstall))
                                            .small()
                                            .on_click(cx.listener(move |this, _, _w, cx| {
                                                this.settings_uninstall_agent_hooks(agent, cx)
                                            })),
                                    )
                                }),
                        )
                        .when_some(row_note, |col, text| {
                            col.child(
                                div()
                                    .max_w_80()
                                    .max_w_full()
                                    .text_xs()
                                    .when(!stacked, |note| note.text_right())
                                    .text_color(muted_fg)
                                    .child(text),
                            )
                        })
                        .into_any_element();

                    page = page.child(self.settings_row(
                        agent.display_name(),
                        row.target,
                        control,
                        cx,
                    ));
                }
            }
        }

        let install_cli_on_path = cx.global::<Config>().install_cli_on_path;
        let cli_switch = crate::ui::theme::switch("install-cli-on-path", cx)
            .checked(install_cli_on_path)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_install_cli_on_path(*on, cx)));
        // Built from the same pieces as every other setting in the app: a
        // section header, then a row whose label and description sit left of
        // its control. Hand-rolled, this was the one switch that stood to the
        // left of its own label — and the one row the settings search could
        // neither highlight nor dim, so a query that counted it in the nav
        // badge left nothing on the page looking like the match.
        page.child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsCommandLine), cx))
            .child(self.settings_row(
                t(L10nKey::SettingsInstallCliOnPath),
                t(L10nKey::SettingsCommandLineDesc),
                cli_switch.into_any_element(),
                cx,
            ))
            .into_any_element()
    }

    fn agent_hooks_machine_picker(&self, selected: HostId, cx: &mut Context<Self>) -> Option<Div> {
        let sf = cx.global::<presets::Surfaces>().window;
        let border = cx.theme().border;
        let muted_fg = cx.theme().muted_foreground;
        let machines = self.agent_hooks_machines(cx);
        let offline = self.agent_hooks_offline_count(cx);
        if machines.len() < 2 && offline == 0 {
            return None;
        }

        Some(
            v_flex()
                .gap_2()
                .mb_4()
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_1p5()
                        .children(machines.into_iter().map(|machine| {
                            let active = machine.host == selected;
                            let host = machine.host;
                            h_flex()
                                .id(("agent-hooks-machine", host.0 as usize))
                                .h(px(24.))
                                .px_2p5()
                                .items_center()
                                .rounded_lg()
                                .border_1()
                                .border_color(border)
                                .bg(rgb(sf.base))
                                .text_sm()
                                .cursor_pointer()
                                .when(active, |s| {
                                    s.bg(rgb(sf.selected))
                                        .text_color(rgb(sf.text_selected))
                                        .font_weight(FontWeight::MEDIUM)
                                })
                                .when(!active, |s| {
                                    s.text_color(rgb(sf.text_resting))
                                        .hover(|h| h.bg(rgb(sf.hover)))
                                })
                                .active(|s| s.bg(rgb(sf.pressed)))
                                .child(machine.label)
                                .on_click(cx.listener(move |this, _, _w, cx| {
                                    this.select_agent_hooks_host(host, cx)
                                }))
                        })),
                )
                .when(offline > 0, |col| {
                    col.child(div().text_xs().text_color(muted_fg).child(t_plural(
                        L10nKey::SettingsOfflineMachines,
                        offline,
                        &[],
                    )))
                }),
        )
    }

    fn render_settings_window_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let cfg = cx.global::<Config>();
        let startup_idx = match cfg.startup_mode {
            crate::core::config::StartupMode::Normal => 0,
            crate::core::config::StartupMode::Maximized => 1,
            crate::core::config::StartupMode::Fullscreen => 2,
        };
        let new_tab_idx = match cfg.new_tab_position {
            NewTabPosition::AfterCurrent => 0,
            NewTabPosition::End => 1,
        };
        let restore_session = cfg.restore_session;
        let remember_window_size = cfg.remember_window_size;
        let show_tray_icon = cfg.show_tray_icon;
        let tab_bar_idx = match cfg.tab_bar_position {
            TabBarPosition::Top => 0,
            TabBarPosition::Left => 1,
        };
        let sidebar_diff_preview = cfg.sidebar_diff_preview;
        let sidebar_grouping_idx = match cfg.sidebar_grouping {
            crate::core::config::SidebarGrouping::Repo => 0,
            crate::core::config::SidebarGrouping::None => 1,
        };
        let notify_idx = match cfg.notify_on_command_finish {
            NotifyMode::Never => 0,
            NotifyMode::Unfocused => 1,
            NotifyMode::Always => 2,
        };
        let threshold_idx = match cfg.notify_threshold_secs {
            n if n <= 5 => 0,
            n if n <= 10 => 1,
            n if n <= 30 => 2,
            _ => 3,
        };
        let notify_radio = self.segmented(
            "wt-notify",
            &[
                t(L10nKey::NotifyModeNever),
                t(L10nKey::NotifyModeUnfocused),
                t(L10nKey::NotifyModeAlways),
            ],
            notify_idx,
            cx,
            |this, ix, _w, cx| {
                let mode = match ix {
                    0 => NotifyMode::Never,
                    1 => NotifyMode::Unfocused,
                    _ => NotifyMode::Always,
                };
                this.set_notify_mode(mode, cx);
            },
        );
        let threshold_radio = self.segmented(
            "wt-notify-threshold",
            &["5s", "10s", "30s", "1m"],
            threshold_idx,
            cx,
            |this, ix, _w, cx| {
                let secs = match ix {
                    0 => 5,
                    1 => 10,
                    2 => 30,
                    _ => 60,
                };
                this.set_notify_threshold(secs, cx);
            },
        );

        let restore_switch = crate::ui::theme::switch("wt-restore-session", cx)
            .checked(restore_session)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_restore_session(*on, cx)))
            .into_any_element();
        let remember_window_switch = crate::ui::theme::switch("wt-remember-window", cx)
            .checked(remember_window_size)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_remember_window_size(*on, cx)))
            .into_any_element();
        let tray_switch = crate::ui::theme::switch("wt-tray-icon", cx)
            .checked(show_tray_icon)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_show_tray_icon(*on, cx)))
            .into_any_element();
        let startup_radio = self.segmented(
            "wt-startup",
            &[
                t(L10nKey::SettingsStartupNormal),
                t(L10nKey::SettingsStartupMaximized),
                t(L10nKey::SettingsStartupFullscreen),
            ],
            startup_idx,
            cx,
            |this, ix, _w, cx| {
                let mode = match ix {
                    0 => crate::core::config::StartupMode::Normal,
                    1 => crate::core::config::StartupMode::Maximized,
                    _ => crate::core::config::StartupMode::Fullscreen,
                };
                this.set_startup_mode(mode, cx);
            },
        );
        let new_tab_radio = self.segmented(
            "wt-new-tab-pos",
            &[t(L10nKey::SettingsAfterCurrent), t(L10nKey::SettingsAtEnd)],
            new_tab_idx,
            cx,
            |this, ix, _w, cx| {
                let pos = if ix == 0 {
                    NewTabPosition::AfterCurrent
                } else {
                    NewTabPosition::End
                };
                this.set_new_tab_position(pos, cx);
            },
        );
        let tab_bar_radio = self.segmented(
            "wt-tab-bar-pos",
            &[t(L10nKey::SettingsTop), t(L10nKey::SettingsLeft)],
            tab_bar_idx,
            cx,
            |this, ix, _w, cx| {
                let pos = if ix == 0 {
                    TabBarPosition::Top
                } else {
                    TabBarPosition::Left
                };
                this.set_tab_bar_position(pos, cx);
            },
        );
        let sidebar_diff_switch = crate::ui::theme::switch("wt-sidebar-diff-preview", cx)
            .checked(sidebar_diff_preview)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_sidebar_diff_preview(*on, cx)))
            .into_any_element();
        let sidebar_grouping_radio = self.segmented(
            "wt-sidebar-grouping",
            &[t(L10nKey::SettingsByRepo), t(L10nKey::SettingsFlat)],
            sidebar_grouping_idx,
            cx,
            |this, ix, _w, cx| {
                let grouping = if ix == 0 {
                    crate::core::config::SidebarGrouping::Repo
                } else {
                    crate::core::config::SidebarGrouping::None
                };
                this.set_sidebar_grouping(grouping, cx);
            },
        );

        v_flex()
            .child(self.section_header(t(L10nKey::SettingsWindow), cx))
            .child(self.settings_row(
                t(L10nKey::SettingsStartupWindow),
                t(L10nKey::SettingsStartupWindowDesc),
                startup_radio,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsRememberWindowSize),
                t(L10nKey::SettingsRememberWindowSizeDesc),
                remember_window_switch,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsRestoreLastLayout),
                t(L10nKey::SettingsRestoreLastLayoutDesc),
                restore_switch,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsShowTrayIcon),
                t(L10nKey::SettingsShowTrayIconDesc),
                tray_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsTabs), cx))
            .child(self.settings_row(
                t(L10nKey::SettingsNewTabPosition),
                t(L10nKey::SettingsNewTabPositionDesc),
                new_tab_radio,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsTabBarPosition),
                t(L10nKey::SettingsTabBarPositionDesc),
                tab_bar_radio,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsSidebarGrouping),
                t(L10nKey::SettingsSidebarGroupingDesc),
                sidebar_grouping_radio,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsDiffPreviewFromCounts),
                t(L10nKey::SettingsDiffPreviewFromCountsDesc),
                sidebar_diff_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsNotifications), cx))
            .child(self.settings_row(
                t(L10nKey::SettingsNotifyOnCommandFinish),
                t(L10nKey::SettingsNotifyOnCommandFinishDesc),
                notify_radio,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsNotifyThreshold),
                t(L10nKey::SettingsNotifyThresholdDesc),
                threshold_radio,
                cx,
            ))
            .into_any_element()
    }

    fn theme_preview(&self, p: &presets::Theme) -> Div {
        let to_u32 = |(r, g, b): (u8, u8, u8)| (r as u32) << 16 | (g as u32) << 8 | b as u32;
        let accent = rgb(p.accent);
        let ansi = |i: usize| rgb(to_u32(p.ansi16[i]));
        let fg = rgb(p.foreground);
        let bar = |frac: f32, color: gpui::Rgba| {
            div().h(px(4.)).w(relative(frac)).rounded(px(1.5)).bg(color)
        };

        v_flex()
            .w_full()
            .bg(rgb(p.background_color()))
            .rounded(rounding::TRACK_RADIUS)
            .overflow_hidden()
            .px_3()
            .py_3()
            .gap(px(10.))
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(px(11.)).text_color(accent).child("❯"))
                    .child(bar(0.5, fg)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(bar(0.2, ansi(2)))
                    .child(bar(0.36, ansi(4)))
                    .child(bar(0.12, ansi(3))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(bar(0.14, ansi(1)))
                    .child(bar(0.44, fg)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(bar(0.1, ansi(6)))
                    .child(bar(0.32, accent)),
            )
    }

    fn render_theme_selection(&self, cx: &mut Context<Self>) -> AnyElement {
        let follow = cx.global::<Config>().theme_follow_system;
        let follow_switch = crate::ui::theme::switch("theme-follow-system", cx)
            .checked(follow)
            .on_click(cx.listener(|this, on: &bool, window, cx| {
                this.set_theme_follow_system(*on, window, cx)
            }))
            .into_any_element();
        let legible = cx.global::<Config>().theme_legible_palette;
        let legible_switch = crate::ui::theme::switch("theme-legible-palette", cx)
            .checked(legible)
            .on_click(cx.listener(|this, on: &bool, window, cx| {
                this.set_theme_legible_palette(*on, window, cx)
            }))
            .into_any_element();
        let root = v_flex()
            .child(self.settings_row(
                t(L10nKey::SettingsSyncWithSystem),
                t(L10nKey::SettingsSyncWithSystemDesc),
                follow_switch,
                cx,
            ))
            .child(self.settings_row(
                t(L10nKey::SettingsLegiblePalette),
                t(L10nKey::SettingsLegiblePaletteDesc),
                legible_switch,
                cx,
            ));
        if follow {
            root.child(self.render_theme_card(ThemeSlot::Light, cx))
                .child(self.render_theme_card(ThemeSlot::Dark, cx))
                .into_any_element()
        } else {
            root.child(self.render_theme_card(ThemeSlot::Manual, cx))
                .into_any_element()
        }
    }

    fn render_theme_card(&self, slot: ThemeSlot, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let border = theme.border;
        let foreground = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let hover_bg = gpui::rgb(cx.global::<presets::Surfaces>().window.hover);
        let surface = theme.secondary.opacity(0.28);

        let config = cx.global::<Config>();
        let (card_id, active_id) = match slot {
            ThemeSlot::Manual => ("theme-card-manual", config.theme_preset.clone()),
            ThemeSlot::Light => ("theme-card-light", config.theme_preset_light.clone()),
            ThemeSlot::Dark => ("theme-card-dark", config.theme_preset_dark.clone()),
        };
        let active = presets::by_id(cx, &active_id);
        let name = active.name.clone();
        let kind = if active.path.is_some() {
            t(L10nKey::SettingsCustom)
        } else {
            t(L10nKey::SettingsBuiltIn)
        };
        let mode = if active.dark {
            t(L10nKey::SettingsDark)
        } else {
            t(L10nKey::SettingsLight)
        };
        let mode_label = if active.dark {
            t(L10nKey::SettingsDarkMode)
        } else {
            t(L10nKey::SettingsLightMode)
        };
        let caption = match slot {
            ThemeSlot::Manual => format!("{kind} · {mode}"),
            ThemeSlot::Light if !crate::ui::theme::system_dark(cx) => {
                format!("{mode_label} · {kind} · {}", t(L10nKey::SettingsActive))
            }
            ThemeSlot::Light => format!("{mode_label} · {kind}"),
            ThemeSlot::Dark if crate::ui::theme::system_dark(cx) => {
                format!("{mode_label} · {kind} · {}", t(L10nKey::SettingsActive))
            }
            ThemeSlot::Dark => format!("{mode_label} · {kind}"),
        };
        let to_u32 = |(r, g, b): (u8, u8, u8)| (r as u32) << 16 | (g as u32) << 8 | b as u32;
        let swatches = h_flex().gap_1().mt_1p5().children((1..=6).map(|i| {
            div()
                .w(px(10.))
                .h(px(10.))
                .rounded(px(3.))
                .bg(rgb(to_u32(active.ansi16[i])))
        }));
        // Inset inside the card's border, the way the same preview is inset
        // inside the same card in the theme panel.
        let preview = self.theme_preview(&active).rounded(rounding::inner_radius(
            rounding::TRACK_RADIUS,
            rounding::HAIRLINE,
        ));
        let open = self
            .active_settings()
            .is_some_and(|s| s.theme_panel_open && s.theme_panel_slot == slot);
        // The same width a row stacks at: the card is a row too, just one whose
        // control happens to be a whole preview.
        let narrow = self.settings_row_under(STACK_ROW_BELOW, cx);

        div()
            .id(card_id)
            .mt_1()
            .mb_2()
            .w_full()
            .cursor_pointer()
            .on_click(
                cx.listener(move |this, _, window, cx| this.toggle_theme_panel(slot, window, cx)),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_4()
                    .p_3()
                    .rounded(rounding::TRACK_RADIUS)
                    .border_1()
                    .border_color(if open {
                        foreground.opacity(0.35)
                    } else {
                        border
                    })
                    .bg(surface)
                    .hover(|h| h.bg(hover_bg))
                    // The preview is the first thing to go: it is a picture of a
                    // choice the two lines beside it already name, and at the
                    // width where it stops fitting it was pushing the "change
                    // theme" affordance off the card.
                    .when(!narrow, |card| {
                        card.child(div().w(px(150.)).flex_shrink_0().child(preview))
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(div().text_xs().text_color(muted_fg).child(caption))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(foreground)
                                    .child(name),
                            )
                            .child(swatches),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap_1()
                            .text_sm()
                            .text_color(muted_fg)
                            .child(t(L10nKey::SettingsChangeTheme))
                            .child(Icon::new(IconName::ChevronRight).small()),
                    ),
            )
            .into_any_element()
    }

    fn render_theme_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let border = theme.border;
        let foreground = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let bg = theme.sidebar;

        let (search, query, slot) = match self.active_settings() {
            Some(s) => (
                s.theme_search.clone(),
                s.theme_search.read(cx).value().trim().to_lowercase(),
                s.theme_panel_slot,
            ),
            None => return div().into_any_element(),
        };
        let list_scroll = self.active_settings().map(|s| s.theme_list_scroll.clone());
        let config = cx.global::<Config>();
        let slot = match (config.theme_follow_system, slot) {
            (false, _) => ThemeSlot::Manual,
            (true, ThemeSlot::Manual) => {
                if crate::ui::theme::system_dark(cx) {
                    ThemeSlot::Dark
                } else {
                    ThemeSlot::Light
                }
            }
            (true, s) => s,
        };
        let active_id = match slot {
            ThemeSlot::Manual => config.theme_preset.clone(),
            ThemeSlot::Light => config.theme_preset_light.clone(),
            ThemeSlot::Dark => config.theme_preset_dark.clone(),
        };

        let header = h_flex()
            .items_center()
            .justify_between()
            .px_4()
            .pt_4()
            .pb_1()
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(foreground)
                    .child(t(L10nKey::SettingsThemes)),
            )
            .child(
                div().occlude().child(
                    Button::new("theme-panel-close")
                        .icon(IconName::Close)
                        .ghost()
                        .small()
                        .tooltip(t(L10nKey::SettingsThemesCloseTooltip))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.close_theme_panel(window, cx)),
                        ),
                ),
            );

        let subtitle = div()
            .px_4()
            .pb_3()
            .text_xs()
            .text_color(muted_fg)
            .child(match slot {
                ThemeSlot::Manual => t(L10nKey::SettingsThemePanelManual),
                ThemeSlot::Light => t(L10nKey::SettingsThemePanelLight),
                ThemeSlot::Dark => t(L10nKey::SettingsThemePanelDark),
            });

        let search_box = div().px_4().pb_3().child(
            div().w_full().child(
                Input::new(&search).small().prefix(
                    Icon::empty()
                        .path("stock/icons/search.svg")
                        .small()
                        .text_color(muted_fg),
                ),
            ),
        );

        // A theme file that fails to parse used to log a warning and then just
        // not be in the list. This is a folder the user opens and drops files
        // into; "it isn't there" needs a reason attached to it.
        let rejected = presets::rejected(cx);
        let rejected_note = (!rejected.is_empty() && query.is_empty()).then(|| {
            let mut note = v_flex()
                .mx_4()
                .mb_4()
                .p_3()
                .gap_1p5()
                .rounded(rounding::TRACK_RADIUS)
                .border_1()
                .border_color(theme.danger.opacity(0.4))
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(foreground)
                        .child(t(L10nKey::SettingsThemesRejected)),
                );
            for (name, why) in &rejected {
                note = note.child(
                    v_flex()
                        .gap_0p5()
                        .child(div().text_xs().text_color(theme.danger).child(name.clone()))
                        .child(div().text_xs().text_color(muted_fg).child(why.clone())),
                );
            }
            note
        });

        let mut list = v_flex().px_4().pb_4().gap_4();
        // Filtering every preset out left the panel blank under its own search
        // box — the one filter in the app that said nothing about it.
        let mut any_themes = false;
        for p in presets::all(cx) {
            if !query.is_empty() && !p.name.to_lowercase().contains(&query) {
                continue;
            }
            any_themes = true;
            let id = p.id.clone();
            let is_active = active_id == id;
            let preview = self.theme_preview(&p).rounded(rounding::inner_radius(
                rounding::TRACK_RADIUS,
                rounding::HAIRLINE,
            ));
            let click_id = id.clone();
            list = list.child(
                v_flex()
                    .id(SharedString::from(format!("panel-theme-{id}")))
                    .gap_1p5()
                    .cursor_pointer()
                    .child(
                        div()
                            .w_full()
                            .rounded(rounding::TRACK_RADIUS)
                            .overflow_hidden()
                            .border_1()
                            .border_color(if is_active {
                                foreground.opacity(0.5)
                            } else {
                                border
                            })
                            .when(is_active, |s| s.shadow_md())
                            .when(!is_active, |s| {
                                s.hover(|h| h.border_color(foreground.opacity(0.25)))
                            })
                            .child(preview),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .w_full()
                            .child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(if is_active {
                                        FontWeight::SEMIBOLD
                                    } else {
                                        FontWeight::MEDIUM
                                    })
                                    .text_color(if is_active { foreground } else { muted_fg })
                                    .child(p.name.clone()),
                            )
                            .when(is_active, |s| {
                                s.child(
                                    Icon::new(IconName::Check)
                                        .small()
                                        .flex_shrink_0()
                                        .text_color(foreground),
                                )
                            }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| match slot {
                        ThemeSlot::Manual => this.set_preset(&click_id, window, cx),
                        ThemeSlot::Light => this.set_slot_preset(false, &click_id, window, cx),
                        ThemeSlot::Dark => this.set_slot_preset(true, &click_id, window, cx),
                    })),
            );
        }

        if !any_themes {
            list = list.child(div().py_2().text_sm().text_color(muted_fg).child(t_fmt(
                L10nKey::SettingsNothingMatches,
                &[("query", query.as_str())],
            )));
        }

        v_flex()
            .w(px(self.settings_columns_now().theme_panel))
            .h_full()
            .flex_shrink_0()
            .bg(bg)
            .border_l_1()
            .border_color(border)
            .child(header)
            .child(subtitle)
            .child(search_box)
            .when_some(list_scroll, |panel, scroll| {
                panel.child(crate::ui::scrollbar::with_vertical_scrollbar(
                    "theme-panel-scrollbar",
                    v_flex()
                        .id("theme-panel-list")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&scroll)
                        .children(rejected_note)
                        .child(list),
                    &scroll,
                ))
            })
            .into_any_element()
    }

    fn render_settings_keybindings(&self, cx: &mut Context<Self>) -> AnyElement {
        let (foreground, muted, border, kbd_bg, accent) = {
            let t = cx.theme();
            (
                t.foreground,
                t.muted_foreground,
                t.border,
                t.secondary.opacity(0.6),
                t.primary,
            )
        };

        let (preset, prefix, overridden) = {
            let cfg = cx.global::<Config>();
            let overridden: std::collections::HashSet<String> =
                cfg.keybindings.keys().cloned().collect();
            (
                cfg.keybinding_preset.clone(),
                cfg.prefix.clone(),
                overridden,
            )
        };
        let tmux = preset == "tmux";
        let effective = crate::ui::keymap::effective_bindings(cx);

        let recording = self
            .active_settings()
            .and_then(|s| s.recording.as_ref())
            .map(|r| (r.action.clone(), r.chords.clone()));
        let record_gen = self.record_gen;
        let note = self
            .active_settings()
            .and_then(|s| s.rebinding_note.clone());

        let keycap = move |tok: String| {
            div()
                .flex()
                .items_center()
                .justify_center()
                .min_w(px(22.))
                .h(px(22.))
                .px_1p5()
                .rounded_md()
                .bg(kbd_bg)
                .border_1()
                .border_color(border)
                .text_xs()
                .text_color(foreground)
                .child(tok)
        };

        let preset_control = self.segmented(
            "kb-preset",
            &[t(L10nKey::SettingsDefault), "tmux"],
            usize::from(tmux),
            cx,
            |this, ix, _w, cx| {
                this.set_keybinding_preset(if ix == 0 { "default" } else { "tmux" }, cx)
            },
        );
        let prefix_control = self.segmented(
            "kb-prefix",
            &["Ctrl-B", "Ctrl-A"],
            usize::from(prefix == "ctrl-a"),
            cx,
            |this, ix, _w, cx| {
                this.set_keybinding_prefix(if ix == 0 { "ctrl-b" } else { "ctrl-a" }, cx)
            },
        );

        // The label column has to be allowed to shrink, or its description sets
        // the row's width and the control it belongs to is pushed off the page.
        // `settings_row` does this for every other row in Settings; these two
        // are hand-rolled and were missing it. They take its breakpoint too:
        // the segmented controls beside them are the widest on the page.
        let stacked = self.settings_row_under(STACK_ROW_BELOW, cx);
        let hand_rolled_row = |row: Div| {
            row.w_full()
                .flex()
                .when(stacked, |r| r.flex_col().items_start().gap_2())
                .when(!stacked, |r| {
                    r.flex_row().items_center().justify_between().gap_8()
                })
        };
        let preset_row = hand_rolled_row(div().py_2())
            .child(
                v_flex()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(foreground)
                            .child(t(L10nKey::SettingsPreset)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(t(L10nKey::SettingsPresetDesc)),
                    ),
            )
            .child(h_flex().flex_shrink_0().child(preset_control));

        let prefix_row = hand_rolled_row(div().py_2())
            .child(
                div()
                    .min_w_0()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(foreground)
                    .child(t(L10nKey::SettingsPrefix)),
            )
            .child(h_flex().flex_shrink_0().child(prefix_control));

        // Eighty-nine undivided rows, in the order the binding table happens to
        // be written. Read them in the same seven sections the command palette
        // uses, so a shortcut is found by where it belongs rather than by
        // scrolling.
        let mut grouped: Vec<(
            crate::ui::palette::CommandGroup,
            Vec<(String, String, String)>,
        )> = Vec::new();
        for (action, key) in effective {
            let (group, label) = crate::ui::keymap::action_entry(&action);
            let slot = match grouped.iter_mut().find(|(g, _)| *g == group) {
                Some(slot) => slot,
                None => {
                    grouped.push((group, Vec::new()));
                    grouped.last_mut().expect("just pushed")
                }
            };
            slot.1.push((action, key, label));
        }
        grouped.sort_by_key(|(g, _)| {
            crate::ui::palette::CommandGroup::ORDER
                .iter()
                .position(|o| o == g)
                .unwrap_or(usize::MAX)
        });
        let rows: Vec<(String, String, String)> = grouped
            .iter()
            .flat_map(|(_, rows)| rows.iter().cloned())
            .collect();
        let heading_at: std::collections::HashMap<usize, &'static str> = {
            let mut map = std::collections::HashMap::new();
            let mut at = 0usize;
            for (group, rows) in &grouped {
                map.insert(at, group.title());
                at += rows.len();
            }
            map
        };
        let count = rows.len();
        let mut list = v_flex().mt_2();
        for (i, (action, key, label)) in rows.into_iter().enumerate() {
            let is_recording = recording.as_ref().is_some_and(|(a, _)| a == &action);
            let is_overridden = overridden.contains(&action);

            // Wrapping, because a four-chord binding is wider than the column a
            // narrow window leaves for it, and the alternative to a second line
            // is a first one that runs off the page.
            let keycaps = |spec: &str| {
                h_flex().flex_wrap().gap_2().children(
                    crate::ui::keymap::key_chords(spec)
                        .into_iter()
                        .map(|chord| h_flex().gap_1().children(chord.into_iter().map(&keycap))),
                )
            };

            let captured: gpui::AnyElement = if is_recording {
                let chords = recording
                    .as_ref()
                    .map(|(_, c)| c.clone())
                    .unwrap_or_default();
                let row = h_flex().gap_2().items_center();
                let row = if chords.is_empty() {
                    row.child(
                        div()
                            .text_xs()
                            .text_color(accent)
                            .child(t(L10nKey::SettingsPressKeys)),
                    )
                } else {
                    // The binding commits on a pause, and the pause was
                    // invisible: the hint asked people to time something they
                    // could not see. The bar runs the same clock the commit
                    // does, and restarts with every extra chord.
                    row.child(keycaps(&chords.join(" "))).child(
                        v_flex()
                            .gap(px(3.))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(t(L10nKey::SettingsPauseToSaveEsc)),
                            )
                            .child(
                                div()
                                    .h(px(2.))
                                    .w_full()
                                    .overflow_hidden()
                                    .rounded_full()
                                    .bg(border)
                                    .child(
                                        div().h_full().rounded_full().bg(accent).with_animation(
                                            ("kb-record-countdown", record_gen as usize),
                                            Animation::new(std::time::Duration::from_millis(
                                                crate::ui::app::RECORD_COMMIT_DELAY_MS,
                                            )),
                                            |bar, delta| bar.w(relative(delta)),
                                        ),
                                    ),
                            ),
                    )
                };
                row.into_any_element()
            } else if key.is_empty() {
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("—")
                    .into_any_element()
            } else {
                keycaps(&key).into_any_element()
            };

            let action_for_click = action.clone();
            let capture = div()
                .id(SharedString::from(format!("kb-{action}")))
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(is_recording, |d| d.border_1().border_color(accent))
                .hover(|d| d.bg(kbd_bg))
                .child(captured)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.start_recording_key(action_for_click.clone(), window, cx)
                }));

            let action_for_reset = action.clone();
            let right = h_flex()
                .items_center()
                .gap_1()
                .child(capture)
                .when(is_overridden, |r| {
                    r.child(
                        Button::new(SharedString::from(format!("reset-{action}")))
                            .label(t(L10nKey::Reset))
                            .small()
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                this.reset_keybinding(action_for_reset.clone(), cx)
                            })),
                    )
                });

            if let Some(title) = heading_at.get(&i) {
                list = list.child(
                    div()
                        .pt_5()
                        .pb_1p5()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(muted)
                        .child(*title),
                );
            }
            let last_in_group = heading_at.contains_key(&(i + 1)) || i + 1 == count;
            list = list.child(
                hand_rolled_row(div().py_1p5())
                    .when(!last_in_group, |s| s.border_b_1().border_color(border))
                    .child(
                        div()
                            .min_w_0()
                            .text_sm()
                            .text_color(foreground)
                            .child(label),
                    )
                    .child(right.flex_shrink_0()),
            );
        }

        v_flex()
            .child(self.section_intro(
                t(L10nKey::SettingsNavKeybindings),
                t(L10nKey::SettingsKeybindingsIntroDesc),
                cx,
            ))
            .child(preset_row)
            .when(tmux, |v| v.child(prefix_row))
            .when(tmux, |v| {
                v.child(
                    div()
                        .py_1()
                        .text_xs()
                        .text_color(muted)
                        .child(t(L10nKey::SettingsPrefixNote)),
                )
            })
            .when_some(note, |v, note| {
                v.child(div().py_1().text_xs().text_color(accent).child(note))
            })
            .child(
                h_flex().justify_end().py_2().child(
                    Button::new("kb-restore-all")
                        .label(t(L10nKey::SettingsRestoreAllDefaults))
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.restore_default_keybindings(window, cx)
                        })),
                ),
            )
            .child(list)
            .into_any_element()
    }

    fn render_settings_about(&self, cx: &mut Context<Self>) -> AnyElement {
        // Copied out rather than held: `self.segmented` below needs `cx`
        // mutably, and a live `cx.theme()` borrow would keep it locked.
        let (foreground, muted_fg, danger) = {
            let theme = cx.theme();
            (theme.foreground, theme.muted_foreground, theme.danger)
        };

        let update_status = cx
            .try_global::<crate::core::update::UpdateStatus>()
            .cloned()
            .unwrap_or_default();
        let update = update_status.available.clone();
        let update_busy = matches!(
            update_status.phase,
            crate::core::update::UpdatePhase::Checking
                | crate::core::update::UpdatePhase::Downloading { .. }
                | crate::core::update::UpdatePhase::Verifying
                | crate::core::update::UpdatePhase::Installing
        );
        let transferring = matches!(
            update_status.phase,
            crate::core::update::UpdatePhase::Downloading { .. }
                | crate::core::update::UpdatePhase::Verifying
        );
        // A staged package whose directory has since been swept away is not an
        // offer worth making.
        let ready = update_status
            .ready
            .clone()
            .filter(crate::core::update::PendingUpdate::is_usable);
        // "You're running the latest version" directly above "27.0.0 is ready
        // to install" is a contradiction, and a reachable one: a release that
        // gets pulled after someone downloaded it leaves exactly this pair.
        // The staged package is the more useful of the two claims.
        let phase_text = localized_update_phase(&update_status.phase).filter(|_| {
            ready.is_none()
                || !matches!(
                    update_status.phase,
                    crate::core::update::UpdatePhase::UpToDate
                )
        });
        let failure = update_status.failure.clone();
        let stale_daemon = crate::daemon::spawn::local_daemon_stale_build();
        // Whether picking up the new build costs the user their running panes
        // decides what this offer is, so it decides what it says.
        let stale_daemon_note = if crate::daemon::spawn::local_daemon_supports(
            crate::daemon::protocol::FEATURE_HANDOFF,
        ) {
            L10nKey::SettingsDaemonStaleDescInPlace
        } else {
            L10nKey::SettingsDaemonStaleDesc
        };
        let check_for_updates = cx.global::<Config>().check_for_updates;
        let auto_download = cx.global::<Config>().auto_download_updates;
        let channel_idx = match cx.global::<Config>().update_channel {
            UpdateChannel::Stable => 0,
            UpdateChannel::Nightly => 1,
        };
        let channel_picker = self.segmented(
            "wt-update-channel",
            &[
                t(L10nKey::SettingsUpdateChannelStable),
                t(L10nKey::SettingsUpdateChannelNightly),
            ],
            channel_idx,
            cx,
            |this, ix, _w, cx| {
                let channel = match ix {
                    0 => UpdateChannel::Stable,
                    _ => UpdateChannel::Nightly,
                };
                this.set_update_channel(channel, cx);
            },
        );
        let http_proxy_input = match self.active_settings() {
            Some(s) => s.http_proxy_input.clone(),
            None => return div().into_any_element(),
        };
        // Only flags a committed value: the input commits on Enter/blur, so a
        // half-typed address is never marked wrong mid-keystroke.
        let http_proxy_value = http_proxy_input.read(cx).value().trim().to_string();
        let http_proxy_invalid = !http_proxy_value.is_empty()
            && !tty7_core::daemon::install::proxy::is_valid_manual(&http_proxy_value);
        let http_proxy_error =
            http_proxy_invalid.then(|| field_error(t(L10nKey::SettingsAppHttpProxyInvalid), cx));
        let http_proxy_control = v_flex()
            .gap_1()
            .w(px(260.))
            .max_w_full()
            .child(Input::new(&http_proxy_input).small())
            .when_some(http_proxy_error, |this, line| this.child(line))
            .into_any_element();

        let logo = Arc::new(Image::from_bytes(
            ImageFormat::Png,
            include_bytes!("../../assets/logo@256.png").to_vec(),
        ));

        v_flex()
            .child(self.section_header(t(L10nKey::SettingsNavAbout), cx))
            .child(
                h_flex()
                    .gap_4()
                    .items_center()
                    .child(img(logo).size_12().rounded_lg())
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(foreground)
                                    .child("tty7"),
                            )
                            .child(div().text_sm().text_color(muted_fg).child(format!(
                                "{} {}",
                                t(L10nKey::SettingsVersion),
                                env!("CARGO_PKG_VERSION")
                            )))
                            .child(
                                Link::new("about-github")
                                    .href("https://github.com/l0ng-ai/tty7")
                                    .text_sm()
                                    .child("github.com/l0ng-ai/tty7"),
                            ),
                    ),
            )
            .child(
                div()
                    .mt_4()
                    .text_sm()
                    .text_color(muted_fg)
                    .child(t(L10nKey::SettingsAboutDesc1)),
            )
            // The search index has promised a "How shells work" entry on this
            // page since it was written, and it pointed at nothing — the one
            // thing that makes tty7 different from any other terminal was
            // never stated in the app.
            .child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsSearchHowShellsWorkTitle), cx))
            .child(
                div()
                    .text_sm()
                    .text_color(muted_fg)
                    .child(t(L10nKey::SettingsHowShellsWorkBody)),
            )
            .child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsUpdates), cx))
            .child(
                v_flex()
                    // The section can carry several stacked states at once —
                    // a failure, a staged package, a skipped version. At gap_2
                    // they read as one paragraph.
                    .gap_3()
                    // A failure the user can act on. Persisted, so it is still
                    // here tomorrow — the old in-memory phase died with the
                    // process and took the only evidence with it.
                    .when_some(failure, |this, failure| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(div().text_sm().text_color(danger).child(t_fmt(
                                    L10nKey::SettingsUpdateFailedTitle,
                                    &[("version", &failure.version)],
                                )))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted_fg)
                                        .child(failure.detail.clone()),
                                )
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Button::new("update-retry")
                                                .label(t(L10nKey::SettingsUpdateRetry))
                                                .small()
                                                .disabled(update_busy)
                                                .on_click(cx.listener(|_, _, _window, cx| {
                                                    crate::core::update::dismiss_failure(cx);
                                                    crate::core::update::install_available(cx);
                                                })),
                                        )
                                        .child(
                                            Button::new("update-manual")
                                                .label(t(L10nKey::SettingsUpdateDownloadManually))
                                                .small()
                                                .on_click(cx.listener(|_, _, _window, _cx| {
                                                    crate::core::update::open_releases_page()
                                                })),
                                        )
                                        .child(
                                            Button::new("update-dismiss")
                                                .label(t(L10nKey::SettingsUpdateDismiss))
                                                .small()
                                                .on_click(cx.listener(|_, _, _window, cx| {
                                                    crate::core::update::dismiss_failure(cx)
                                                })),
                                        ),
                                ),
                        )
                    })
                    // Downloaded and verified: the decision left is "may I
                    // restart", not "will you spend five minutes on this".
                    .when_some(ready.clone(), |this, pending| {
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(div().text_sm().text_color(foreground).child(t_fmt(
                                    L10nKey::SettingsUpdateReady,
                                    &[("version", &pending.version)],
                                )))
                                .when(pending.apply_on_launch, |this| {
                                    this.child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child(t(L10nKey::SettingsUpdateReadyNextLaunch)),
                                    )
                                })
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Button::new("install-ready")
                                                .label(t(L10nKey::SettingsUpdateInstallNow))
                                                .small()
                                                .disabled(update_busy)
                                                .on_click(cx.listener(|_, _, _window, cx| {
                                                    crate::core::update::install_available(cx)
                                                })),
                                        )
                                        .child(
                                            Button::new("discard-ready")
                                                .label(t(L10nKey::SettingsUpdateDiscard))
                                                .small()
                                                .disabled(update_busy)
                                                .on_click(cx.listener(|_, _, _window, cx| {
                                                    crate::core::update::discard_pending(cx)
                                                })),
                                        ),
                                ),
                        )
                    })
                    // One action, not the three that used to crowd this row.
                    // The update dialog covers the rest, but it is a moment
                    // rather than a place: after "Later" it does not come back
                    // for days, and where the package cannot be installed for
                    // the user — Linux, an unsupported install — the release
                    // page is the whole update path. That cannot live only in a
                    // dialog that has already been dismissed.
                    .when_some(update.filter(|_| ready.is_none()), |this, upd| {
                        let availability = t_fmt(
                            L10nKey::SettingsVersionAvailable,
                            &[("version", &upd.version)],
                        );
                        // `install_available` opens the release page by itself
                        // when there is nothing to install, so both labels lead
                        // to the one call.
                        let action = if upd.installable {
                            t(L10nKey::SettingsUpdateAndRelaunch)
                        } else {
                            t(L10nKey::SettingsUpdateViewRelease)
                        };
                        this.child(
                            v_flex()
                                .gap_2()
                                .child(
                                    h_flex()
                                        .gap_3()
                                        .items_center()
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(foreground)
                                                .child(availability),
                                        )
                                        .child(
                                            Button::new("install-update")
                                                .label(action)
                                                .small()
                                                .disabled(update_busy)
                                                .on_click(cx.listener(|_, _, _window, cx| {
                                                    crate::core::update::install_available(cx)
                                                })),
                                        ),
                                )
                                .when_some(upd.install_hint, |this, hint| {
                                    this.child(
                                        div()
                                            .text_xs()
                                            .text_color(muted_fg)
                                            .child(localized_update_install_hint(&hint)),
                                    )
                                }),
                        )
                    })
                    .when_some(phase_text, |this, text| {
                        this.child(div().text_sm().text_color(muted_fg).child(text))
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("check-update-now")
                                    .label(
                                        if matches!(
                                            update_status.phase,
                                            crate::core::update::UpdatePhase::Checking
                                        ) {
                                            t(L10nKey::SettingsUpdateChecking)
                                        } else {
                                            t(L10nKey::SettingsUpdateCheckNow)
                                        },
                                    )
                                    .small()
                                    .disabled(update_busy)
                                    .on_click(cx.listener(|_, _, _window, cx| {
                                        crate::core::update::spawn_check_forced(cx)
                                    })),
                            )
                            // Thirty megabytes on a slow link is exactly the
                            // download someone wants to call off; without this
                            // the only way out was to kill the app.
                            .when(transferring, |this| {
                                this.child(
                                    Button::new("cancel-update-download")
                                        .label(t(L10nKey::SettingsUpdateCancel))
                                        .small()
                                        .on_click(cx.listener(|_, _, _window, cx| {
                                            crate::core::update::cancel_download(cx)
                                        })),
                                )
                            }),
                    )
                    .child(self.settings_row(
                        t(L10nKey::SettingsUpdateChannel),
                        t(L10nKey::SettingsUpdateChannelDesc),
                        channel_picker,
                        cx,
                    ))
                    .child(
                        self.settings_row(
                            t(L10nKey::SettingsCheckUpdatesOnLaunch),
                            t(L10nKey::SettingsCheckUpdatesDesc),
                            crate::ui::theme::switch("check-updates", cx)
                                .checked(check_for_updates)
                                .on_click(cx.listener(|this, on: &bool, _w, cx| {
                                    this.set_check_for_updates(*on, cx)
                                }))
                                .into_any_element(),
                            cx,
                        ),
                    )
                    .child(
                        self.settings_row(
                            t(L10nKey::SettingsAutoDownload),
                            t(L10nKey::SettingsAutoDownloadDesc),
                            crate::ui::theme::switch("auto-download-updates", cx)
                                .checked(auto_download)
                                .on_click(cx.listener(|this, on: &bool, _w, cx| {
                                    this.set_auto_download_updates(*on, cx)
                                }))
                                .into_any_element(),
                            cx,
                        ),
                    ),
            )
            .child(self.settings_row(
                t(L10nKey::SettingsAppHttpProxy),
                t(L10nKey::SettingsAppHttpProxyDesc),
                http_proxy_control,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header(t(L10nKey::SettingsServer), cx))
            .child(
                v_flex()
                    .gap_2()
                    // The other half of an in-place update: the app is new, the
                    // process serving every pane is not. Said here rather than
                    // beside the update controls, so the one button that offers
                    // to pick the new build up stays the only one on the page.
                    .when_some(stale_daemon.as_deref(), |this, build| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(foreground)
                                .child(t_fmt(L10nKey::SettingsDaemonStale, &[("build", build)])),
                        )
                    })
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted_fg)
                            // A stale server has a more specific thing to say
                            // than the section's standing description, and it
                            // ends with the same button.
                            .child(t(if stale_daemon.is_some() {
                                stale_daemon_note
                            } else {
                                L10nKey::SettingsServerDesc
                            })),
                    )
                    .child(
                        h_flex().child(
                            Button::new("restart-daemon")
                                .label(t(L10nKey::SettingsRestartServer))
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.restart_daemon(window, cx)
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row keeps its side-by-side shape while both halves fit, and stacks
    /// once they do not. The SSH page reaches that point first — it spends its
    /// host list before the row gets anything.
    #[test]
    fn a_row_stacks_once_its_label_and_control_stop_fitting() {
        use SettingsSection::*;
        assert!(settings_row_width(Terminal, false, 1440., 1.) >= STACK_ROW_BELOW);
        assert!(settings_row_width(Terminal, false, 900., 1.) >= STACK_ROW_BELOW);
        // At the narrowest window that turns up the page is 420 wide, which is
        // under the width where a label and a control still share a line.
        assert!(settings_row_width(Terminal, false, NARROWEST_WINDOW, 1.) < STACK_ROW_BELOW);
        // Capped at the reading column, so a wider window never widens the row.
        assert_eq!(
            settings_row_width(Terminal, false, 4000., 1.),
            READING_COLUMN
        );
        // SSH crosses over while the window is still wide — it is the page that
        // spends a host list before its rows get anything.
        assert!(settings_row_width(Ssh, false, 1440., 1.) >= STACK_ROW_BELOW);
        assert!(settings_row_width(Ssh, false, 900., 1.) < STACK_ROW_BELOW);
        // And never goes negative on a window narrower than its own chrome.
        assert_eq!(settings_row_width(Ssh, false, 100., 1.), 0.);
    }

    /// Every list gives width back before the page does, and no combination of
    /// page and panel leaves the page below the width it is derived to keep.
    /// 641pt is the window the report came from — under the declared 720 —- so
    /// that is the case that has to hold, not the one the manifest promises.
    #[test]
    fn the_page_keeps_a_readable_width_at_every_window_that_turns_up() {
        use SettingsSection::*;
        for section in SettingsSection::ALL {
            for panel_open in [false, true] {
                for viewport in [NARROWEST_WINDOW, 641., 720., 900., 1100., 1440., 2560.] {
                    let cols = settings_columns(section, panel_open, viewport);
                    let pad = match section {
                        Ssh => SSH_DETAIL_PAD,
                        _ => PAGE_PAD,
                    };
                    let panel = only_when(!cols.panel_overlays, cols.theme_panel);
                    let page = viewport - cols.nav - cols.ssh_list - panel - pad;
                    assert!(
                        page >= CONTENT_MIN_W,
                        "{viewport}px, {panel_open}: page got {page}, floor is {CONTENT_MIN_W}"
                    );
                    // And the columns add up to the window rather than past it,
                    // which is what keeps the rightmost one on screen.
                    assert!(cols.nav + cols.ssh_list + panel + pad + page <= viewport + 1.);
                    assert!(cols.nav >= NAV_W_MIN && cols.nav <= NAV_W);
                }
            }
        }
    }

    /// The three screenshots in the report, by the numbers measured off them.
    /// Every one of them is the same 641pt window.
    #[test]
    fn the_reported_window_lays_out_without_running_off_the_screen() {
        use SettingsSection::*;
        const REPORTED: f32 = 641.;

        // Shot 1 — SSH. Nav 220 and host list 280 left the detail 141pt and its
        // empty state painted ~270 past the window edge. Both lists now stand
        // on their floors and the detail keeps the rest.
        let ssh = settings_columns(Ssh, false, REPORTED);
        assert_eq!((ssh.nav, ssh.ssh_list), (NAV_W_MIN, SSH_LIST_W_MIN));
        let detail = settings_row_width(Ssh, false, REPORTED, 1.);
        assert!(detail >= CONTENT_MIN_W, "SSH detail got {detail}");

        // Shot 2 — Appearance, panel closed. The opacity row measured a 78pt
        // label beside a 240pt slider; at this width the row has to stack.
        // The page does not reach its preferred `CONTENT_W` here: the nav floor
        // is sized to show a Japanese nav label whole, and at 641 that costs the
        // page the difference. Readable and stacked is the property that matters.
        let page = settings_row_width(Appearance, false, REPORTED, 1.);
        assert_eq!(page, REPORTED - NAV_W_MIN - PAGE_PAD);
        assert!(page >= CONTENT_MIN_W, "the page got {page}");
        assert!(
            page < STACK_ROW_BELOW,
            "the opacity row has to stack at 641"
        );

        // Shot 3 — Appearance with the theme panel. 220 + 300 of chrome left
        // the page ~125pt, one Chinese character per line. The panel now lifts
        // off the row entirely and the page is back to shot 2's width.
        assert!(settings_columns(Appearance, true, REPORTED).panel_overlays);
        assert_eq!(settings_row_width(Appearance, true, REPORTED, 1.), page);
    }

    /// The list that has to give the most is the one the window has the least
    /// room for, and no list is ever asked for more than it has to spare.
    #[test]
    fn the_lists_shrink_together_and_stop_at_their_floors() {
        use SettingsSection::*;
        // Wide enough for everyone: nothing moves.
        let wide = settings_columns(Ssh, false, 1440.);
        assert_eq!((wide.nav, wide.ssh_list), (NAV_W, SSH_LIST_W));
        // The reported window — half of a 1440pt screen, three columns on SSH.
        // Both lists give, neither past its floor, and the detail comes out at
        // its preferred width instead of the 336 it used to be left with.
        let half = settings_columns(Ssh, false, 900.);
        assert!(half.nav < NAV_W && half.ssh_list < SSH_LIST_W);
        assert!(half.nav >= NAV_W_MIN && half.ssh_list >= SSH_LIST_W_MIN);
        assert_eq!(
            settings_row_width(Ssh, false, 900., 1.).round(),
            CONTENT_W,
            "the SSH detail should get its preferred width at 900pt"
        );
        // The narrowest window that turns up: both at the floor, page readable.
        let tiny = settings_columns(Ssh, false, NARROWEST_WINDOW);
        assert_eq!((tiny.nav, tiny.ssh_list), (NAV_W_MIN, SSH_LIST_W_MIN));
        assert_eq!(
            settings_row_width(Ssh, false, NARROWEST_WINDOW, 1.),
            CONTENT_MIN_W
        );
    }

    /// The thresholds are widths a *label* needs, and a reader who scaled the
    /// interface up scaled every label with it while the slider beside it kept
    /// the px width it was built at. A window that reads fine at the default
    /// font is a starved label column at the largest one.
    #[test]
    fn the_stacking_width_follows_the_interface_font() {
        use crate::core::config::UI_FONT_SIZE_MAX;
        use SettingsSection::*;
        let large = UI_FONT_SIZE_MAX / UI_FONT_SIZE_DEFAULT;
        // Side by side at the default font...
        assert!(settings_row_width(Appearance, false, 900., 1.) >= STACK_ROW_BELOW);
        // ...and stacked at the largest, where the same row holds half as much.
        assert!(
            settings_row_width(Appearance, false, 900., large) < STACK_ROW_BELOW * large,
            "a 900pt window at the largest interface font has to stack"
        );
        // The reading column grows with the font, so a wide window does not.
        assert!(settings_row_width(Appearance, false, 1600., large) >= STACK_ROW_BELOW * large);
    }

    /// The theme panel took its 300px from the page and from nothing else, so
    /// a half-width window with it open rendered a description one character
    /// wide. It now shrinks with everything else, and stops being a column at
    /// all once even that is not enough.
    #[test]
    fn the_theme_panel_yields_before_the_page_does() {
        use SettingsSection::*;
        assert!(settings_row_width(Appearance, true, 900., 1.) < STACK_ROW_BELOW);
        assert!(
            settings_row_width(Appearance, true, 900., 1.)
                < settings_row_width(Appearance, false, 900., 1.)
        );
        // Beside the page while both fit — which, with the panel and the nav
        // both allowed down to their floors, still holds at 720.
        assert!(!settings_columns(Appearance, true, 900.).panel_overlays);
        assert!(!settings_columns(Appearance, true, 720.).panel_overlays);
        // ...and over it once they do not, at which point the page is back to
        // the width it has with the panel closed.
        assert!(settings_columns(Appearance, true, 641.).panel_overlays);
        assert_eq!(
            settings_row_width(Appearance, true, 641., 1.),
            settings_row_width(Appearance, false, 641., 1.)
        );
        // The panel is the only chrome that can leave, so it has to leave in
        // time: at 641 there is no arrangement in which it and a readable page
        // both fit in a row.
        assert!(
            NARROWEST_WINDOW - NAV_W_MIN - THEME_PANEL_W_MIN - PAGE_PAD < CONTENT_MIN_W,
            "the overlay threshold has to fire at the narrowest window"
        );
        // Wide enough and the cap is the reading column either way.
        assert_eq!(
            settings_row_width(Appearance, true, 1600., 1.),
            READING_COLUMN
        );
    }

    #[test]
    fn a_row_is_marked_by_its_label_or_by_the_keywords_behind_it() {
        // Straight label hit.
        assert!(row_matches_query(
            SettingsSection::Appearance,
            "Blur",
            "blur"
        ));
        // Keyword hit: nothing on the About page contains "persist", but the
        // index says the "How shells work" block answers it.
        assert!(row_matches_query(
            SettingsSection::About,
            t(L10nKey::SettingsSearchHowShellsWorkTitle),
            "persist"
        ));
        // A row on some other page is not a hit just because the query matches
        // an entry elsewhere.
        assert!(!row_matches_query(
            SettingsSection::Terminal,
            "Blur",
            "persist"
        ));
        // An empty query marks nothing at all, so no page ever renders greyed
        // out just because the field is focused.
        assert!(!row_matches_query(SettingsSection::Appearance, "Blur", ""));
    }

    #[test]
    fn a_query_that_matches_nothing_is_distinguishable_from_one_that_does() {
        assert_eq!(total_match_count("zzqqxx"), 0);
        assert!(total_match_count("blur") > 0);
        assert!(total_match_count("persist") > 0);
    }

    #[test]
    fn the_how_shells_work_entry_has_something_to_point_at() {
        // The index promised this page an explanation of the one thing that
        // makes tty7 different; for a long time it pointed at nothing.
        assert!(
            !t(L10nKey::SettingsHowShellsWorkBody).is_empty(),
            "the About page has no body copy for its own search entry"
        );
        assert_eq!(
            best_matching_section("persist").map(|s| s.profile_label()),
            Some(SettingsSection::About.profile_label())
        );
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
            ("completion", Input),
            ("ctrl-r", Input),
            ("grouping", WindowTabs),
            ("threshold", WindowTabs),
            ("report mouse", Terminal),
            ("nushell", Terminal),
            ("open files with", Terminal),
            ("bell", Terminal),
            ("known_hosts", Ssh),
            ("claude", Agents),
            ("symlink", Agents),
            // Rows the index had no entry for at all, so the query counted
            // nothing, no badge appeared and no row lit up: the whole Updates
            // group on About, and Smooth scrolling between two rows that were
            // both findable.
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
            "Start in",
            "Restore last layout",
            "Terminal bell",
            "Report mouse to apps",
            "Open files with",
            "Sidebar grouping",
            "Tab completion",
            "History search",
            "Dim inactive panes",
            "Option (⌥) acts as Meta",
            "Install the tty7 command on PATH",
        ] {
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
    use crate::core::config::Config;
    use crate::core::session::Session;
    use crate::ui::app::Tty7App;
    use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext, px, size};

    fn harness(cx: &mut TestAppContext) -> (Entity<Tty7App>, VisualTestContext) {
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

    #[gpui::test]
    fn appearance_section_lays_out_with_its_rounded_controls(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::Appearance, window, cx);
        });

        vcx.simulate_resize(size(px(1100.), px(800.)));
        vcx.run_until_parked();

        app.update_in(&mut vcx, |app, _, cx| {
            if let Some(s) = app.active_settings_mut() {
                s.theme_panel_open = true;
            }
            cx.notify();
        });
        vcx.simulate_resize(size(px(720.), px(560.)));
        vcx.run_until_parked();

        let section = vcx.update(|_, cx| app.read(cx).active_settings().map(|s| s.section));
        assert!(
            matches!(section, Some(SettingsSection::Appearance)),
            "the panel should still be on Appearance after two paint passes",
        );
    }
}

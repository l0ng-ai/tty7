use gpui::{
    AnyElement, App, Context, Div, Entity, FontWeight, Image, ImageFormat, KeyDownEvent,
    MouseButton, SharedString, Stateful, Subscription, Window, div, img, prelude::*, px, relative,
    rgb,
};
use gpui_component::InteractiveElementExt as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::color_picker::{ColorPicker, ColorPickerState};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::link::Link;
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::select::{SearchableVec, Select, SelectState};
use gpui_component::sidebar::{Sidebar, SidebarCollapsible, SidebarMenu, SidebarMenuItem};
use gpui_component::slider::{Slider, SliderState};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex,
    v_flex,
};
use std::sync::Arc;

use uuid::Uuid;

use crate::core::config::{
    BellMode, Config, CursorStyle, NewTabPosition, NotifyMode, TabBarPosition,
};
use crate::core::keychain::CredentialRef;
use crate::core::ssh_profile::{
    Algorithms, AuthMode, ForwardKind, ForwardRule, HostPort, SshProfile, to_connect_string,
};
use crate::ui::app::{
    FONT_SIZE_STEP, LINE_HEIGHT_STEP, TILE_GLYPH_LINE, TILE_SIZE, TITLE_BAR_HEIGHT, ThemeEdit,
    Tty7App,
};
use crate::ui::host_ops::HostId;
use crate::ui::presets;
use crate::ui::rounding;
use crate::ui::rounding::RoundedCorners as _;

fn settings_row_id(label: &str, _desc: &str) -> SharedString {
    SharedString::from(format!("settings-row-{label}"))
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
    title: &'static str,
    keywords: &'static str,
}

fn settings_search_entries() -> &'static [SearchEntry] {
    use SettingsSection::*;
    &[
        SearchEntry {
            section: Appearance,
            title: "Theme",
            keywords: "appearance color colours scheme dark light palette background foreground accent sync system os auto follow",
        },
        SearchEntry {
            section: Appearance,
            title: "Sync with system",
            keywords: "theme dark light auto follow os appearance mode",
        },
        SearchEntry {
            section: Appearance,
            title: "Custom themes",
            keywords: "theme duplicate edit colors folder yaml import",
        },
        SearchEntry {
            section: Appearance,
            title: "Opacity",
            keywords: "transparency translucent see through window alpha",
        },
        SearchEntry {
            section: Appearance,
            title: "Blur",
            keywords: "transparency translucent frosted vibrancy window background",
        },
        SearchEntry {
            section: Appearance,
            title: "Dim inactive panes",
            keywords: "fade unfocused inactive split pane focus opacity highlight active dimming",
        },
        SearchEntry {
            section: Appearance,
            title: "Font size",
            keywords: "typography text bigger smaller zoom",
        },
        SearchEntry {
            section: Appearance,
            title: "Line height",
            keywords: "typography leading spacing",
        },
        SearchEntry {
            section: Appearance,
            title: "Font family",
            keywords: "typeface monospace typography",
        },
        SearchEntry {
            section: Appearance,
            title: "Bold font",
            keywords: "typeface weight",
        },
        SearchEntry {
            section: Appearance,
            title: "Italic font",
            keywords: "typeface oblique",
        },
        SearchEntry {
            section: Appearance,
            title: "Font ligatures",
            keywords: "typography glyph fira",
        },
        SearchEntry {
            section: Appearance,
            title: "Cursor shape",
            keywords: "caret block bar underline beam",
        },
        SearchEntry {
            section: Appearance,
            title: "Cursor blink",
            keywords: "caret blinking flash",
        },
        SearchEntry {
            section: Appearance,
            title: "ANSI colors",
            keywords: "palette 16 terminal colours theme",
        },
        SearchEntry {
            section: Terminal,
            title: "Program",
            keywords: "shell binary zsh bash fish nu nushell pwsh powershell executable launch",
        },
        SearchEntry {
            section: Terminal,
            title: "Arguments",
            keywords: "shell flags login args",
        },
        SearchEntry {
            section: Terminal,
            title: "Start in",
            keywords: "cwd working directory start folder path home inherit custom",
        },
        SearchEntry {
            section: Terminal,
            title: "Scrollback",
            keywords: "history buffer lines scroll",
        },
        SearchEntry {
            section: Terminal,
            title: "Scroll speed",
            keywords: "mouse wheel multiplier scrolling",
        },
        SearchEntry {
            section: Terminal,
            title: "Focus follows mouse",
            keywords: "pane hover activate",
        },
        SearchEntry {
            section: Terminal,
            title: "Hide mouse while typing",
            keywords: "cursor pointer autohide",
        },
        SearchEntry {
            section: Terminal,
            title: "Report mouse to apps",
            keywords: "mouse reporting vim tmux click scroll shift passthrough",
        },
        SearchEntry {
            section: Terminal,
            title: "Terminal bell",
            keywords: "bell audible visual flash sound silence beep ^g",
        },
        SearchEntry {
            section: Terminal,
            title: "Detect URLs",
            keywords: "links hyperlink clickable open",
        },
        SearchEntry {
            section: Terminal,
            title: "Forward SSH loopback links",
            keywords: "ssh remote port tunnel localhost forward links",
        },
        SearchEntry {
            section: Terminal,
            title: "Open files with",
            keywords: "links file editor command external app path line column",
        },
        SearchEntry {
            section: Input,
            title: "Tab completion",
            keywords: "complete completion menu suggestions tab prompt",
        },
        SearchEntry {
            section: Input,
            title: "History search",
            keywords: "ctrl-r reverse search fuzzy history recall fzf prompt",
        },
        SearchEntry {
            section: Input,
            title: "Option (⌥) acts as Meta",
            keywords: "alt keyboard modifier escape macos option meta option acts as meta",
        },
        SearchEntry {
            section: Input,
            title: "Smart selection",
            keywords: "double click word url path select semantic bracket email",
        },
        SearchEntry {
            section: Input,
            title: "Copy on select",
            keywords: "clipboard selection yank mouse",
        },
        SearchEntry {
            section: Input,
            title: "Trim trailing spaces on copy",
            keywords: "clipboard whitespace copy",
        },
        SearchEntry {
            section: Ssh,
            title: "Hosts",
            keywords: "ssh host connection saved profile import ssh_config manage add edit \
                       quick connect",
        },
        SearchEntry {
            section: Ssh,
            title: "Verify host keys",
            keywords: "ssh security known_hosts fingerprint mitm host key verification",
        },
        SearchEntry {
            section: Ssh,
            title: "Warn before closing",
            keywords: "ssh confirm close tab pane live session security",
        },
        SearchEntry {
            section: Ssh,
            title: "Port forwarding",
            keywords: "ssh tunnel local remote dynamic socks forward rule",
        },
        SearchEntry {
            section: Agents,
            title: "Claude Code",
            keywords: "agent integration hooks install uninstall status rich session working waiting tab bar sidebar badge claude",
        },
        SearchEntry {
            section: Agents,
            title: "Codex",
            keywords: "agent integration hooks install openai codex",
        },
        SearchEntry {
            section: Agents,
            title: "Copilot CLI",
            keywords: "agent integration hooks install github copilot",
        },
        SearchEntry {
            section: Agents,
            title: "OpenCode",
            keywords: "agent integration plugin install opencode",
        },
        SearchEntry {
            section: Agents,
            title: "Pi",
            keywords: "agent integration extension install pi",
        },
        SearchEntry {
            section: Agents,
            title: "Grok Build",
            keywords: "agent integration hooks install xai grok build",
        },
        SearchEntry {
            section: WindowTabs,
            title: "Startup window",
            keywords: "launch open maximized fullscreen normal",
        },
        SearchEntry {
            section: WindowTabs,
            title: "Remember window size & position",
            keywords: "window size position bounds geometry launch startup remember",
        },
        SearchEntry {
            section: WindowTabs,
            title: "Restore last layout",
            keywords: "restore session previous tabs splits reopen launch startup layout",
        },
        SearchEntry {
            section: WindowTabs,
            title: "Confirm before closing the last window",
            keywords: "close quit confirm prompt dialog ask again warn last window cmd-w ctrl-w",
        },
        SearchEntry {
            section: WindowTabs,
            title: "Show tray icon",
            keywords: "tray menu bar status item agent attention system icon",
        },
        SearchEntry {
            section: WindowTabs,
            title: "New tab position",
            keywords: "tabs order end after current",
        },
        SearchEntry {
            section: WindowTabs,
            title: "Tab bar position",
            keywords: "tabs vertical sidebar left top layout rail",
        },
        SearchEntry {
            section: WindowTabs,
            title: "Sidebar grouping",
            keywords: "tabs group repo repository git scratch header sidebar flat",
        },
        SearchEntry {
            section: WindowTabs,
            title: "Open diff preview from sidebar counts",
            keywords: "diff overlay preview sidebar counts git changes click branch lines",
        },
        SearchEntry {
            section: WindowTabs,
            title: "Notify on command finish",
            keywords: "notification alert done osc desktop banner long command",
        },
        SearchEntry {
            section: WindowTabs,
            title: "Notify threshold",
            keywords: "notification alert seconds duration long command delay",
        },
        SearchEntry {
            section: Keybindings,
            title: "Keybindings",
            keywords: "shortcut hotkey keyboard binding chord tmux preset rebind prefix",
        },
        SearchEntry {
            section: About,
            title: "About",
            keywords: "version license credits build update check github",
        },
        SearchEntry {
            section: About,
            title: "How shells work",
            keywords: "shell session daemon server detach persist background close quit stop delete workspace layout survive reboot tmux",
        },
        SearchEntry {
            section: About,
            title: "Command line tool",
            keywords: "cli tty7 path shell command install symlink terminal iterm agent script",
        },
        SearchEntry {
            section: About,
            title: "Windows Explorer context menu",
            keywords: "windows explorer right click folder directory background shell menu register unregister open here",
        },
    ]
}

fn entry_matches(entry: &SearchEntry, query: &str) -> bool {
    entry.title.to_lowercase().contains(query) || entry.keywords.contains(query)
}

pub(crate) fn section_match_count(section: SettingsSection, query: &str) -> usize {
    settings_search_entries()
        .iter()
        .filter(|e| e.section == section && entry_matches(e, query))
        .count()
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
    pub(crate) seed: Vec<(ThemeEdit, String, Entity<ColorPickerState>)>,
    pub(crate) ansi: Vec<(ThemeEdit, String, Entity<ColorPickerState>)>,
    pub(crate) image_opacity_slider: Option<Entity<SliderState>>,
    pub(crate) _subs: Vec<Subscription>,
}

pub(crate) struct SettingsState {
    pub(crate) focus_handle: gpui::FocusHandle,
    pub(crate) section: SettingsSection,
    pub(crate) search: Entity<InputState>,
    pub(crate) font_select: Entity<SelectState<SearchableVec<String>>>,
    pub(crate) font_bold_select: Entity<SelectState<SearchableVec<String>>>,
    pub(crate) font_italic_select: Entity<SelectState<SearchableVec<String>>>,
    pub(crate) shell_program_input: Entity<InputState>,
    pub(crate) shell_args_input: Entity<InputState>,
    pub(crate) wd_path_input: Entity<InputState>,
    pub(crate) link_file_command_input: Entity<InputState>,
    pub(crate) scroll_slider: Entity<SliderState>,
    pub(crate) window_opacity_slider: Entity<SliderState>,
    pub(crate) theme_editor: Option<ThemeEditor>,
    pub(crate) theme_panel_open: bool,
    pub(crate) theme_panel_slot: ThemeSlot,
    pub(crate) theme_search: Entity<InputState>,
    pub(crate) recording: Option<Recording>,
    pub(crate) rebinding_note: Option<String>,
    pub(crate) explorer_context_menu_status:
        Result<crate::core::explorer_context_menu::Status, String>,
    pub(crate) explorer_context_menu_note: Option<String>,
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
        "" => "In tty7",
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

pub(crate) const FONT_DEFAULT_LABEL: &str = "Default (match primary)";

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

fn parse_host_port(s: &str) -> Option<HostPort> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    match s.rsplit_once(':') {
        Some((h, p)) => Some(HostPort::new(h.trim(), p.trim().parse().unwrap_or(0))),
        None => Some(HostPort::new(s, 0)),
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
        description: seed_hinted(window, cx, &rule.description, "what it's for"),
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
        let (background, foreground, header_muted) =
            (theme.background, theme.foreground, theme.muted_foreground);

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
                "Appearance",
                SettingsSection::Appearance,
                Icon::new(IconName::Palette),
            ))
            .child(nav_item(
                "Terminal",
                SettingsSection::Terminal,
                Icon::new(IconName::SquareTerminal),
            ))
            .child(nav_item(
                "Input",
                SettingsSection::Input,
                Icon::new(IconName::Settings2),
            ))
            .child(nav_item(
                "SSH",
                SettingsSection::Ssh,
                Icon::new(IconName::Globe),
            ))
            .child(nav_item(
                "Agents",
                SettingsSection::Agents,
                Icon::new(IconName::Bot),
            ))
            .child(nav_item(
                "Window & Tabs",
                SettingsSection::WindowTabs,
                Icon::new(IconName::WindowRestore),
            ))
            .child(nav_item(
                "Keybindings",
                SettingsSection::Keybindings,
                Icon::new(IconName::CaseSensitive),
            ))
            .child(nav_item(
                "About",
                SettingsSection::About,
                Icon::empty().path("icons/circle-info.svg"),
            ));

        let sidebar = Sidebar::new("settings-sidebar")
            .collapsible(SidebarCollapsible::None)
            .w(px(220.))
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
                            .child("SETTINGS"),
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

        let content_pane = if section == SettingsSection::Ssh {
            v_flex()
                .id("settings-content")
                .flex_1()
                .min_w_0()
                .h_full()
                .bg(background)
                .child(content)
        } else {
            v_flex()
                .id("settings-content")
                .flex_1()
                .min_w_0()
                .h_full()
                .bg(background)
                .overflow_y_scroll()
                .child(
                    div()
                        .px_10()
                        .py_8()
                        .child(div().w_full().max_w(px(640.)).child(content)),
                )
        };

        let root = div()
            .size_full()
            .relative()
            .flex()
            .flex_row()
            .bg(background)
            .text_color(foreground)
            .track_focus(&focus_handle)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key.as_str() == "escape" {
                    this.close_settings(window, cx);
                }
            }))
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
            .when(show_theme_panel, |r| r.child(self.render_theme_panel(cx)))
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
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_settings(window, cx)
                                })),
                        ),
                )
            });

        if let Some((start, label)) = prof {
            crate::ui::perf::record(label, start.elapsed());
        }
        root
    }

    fn header_text(&self, title: &str, cx: &Context<Self>) -> Div {
        div()
            .text_base()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().foreground)
            .child(title.to_string())
    }

    pub(crate) fn section_header(&self, title: &str, cx: &Context<Self>) -> Div {
        self.header_text(title, cx).mb_4()
    }

    fn section_intro(&self, title: &str, desc: impl Into<String>, cx: &Context<Self>) -> Div {
        v_flex()
            .mb_4()
            .gap_1()
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
        h_flex()
            .id(element_id)
            .items_center()
            .justify_between()
            .gap_8()
            .py_2()
            .px_2p5()
            .mx_neg_2p5()
            .rounded_lg()
            .hover(|h| h.bg(gpui::rgb(cx.global::<presets::Surfaces>().window.hover)))
            .on_hover(cx.listener(|_this, _hovered, _window, cx| cx.notify()))
            .child(
                v_flex()
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
                    }),
            )
            .child(h_flex().flex_shrink_0().child(control))
    }

    pub(crate) fn segmented(
        &self,
        id: impl Into<SharedString>,
        options: &'static [&'static str],
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
        options: &'static [&'static str],
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
                    .child(*label)
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
        let (font_select, font_bold_select, font_italic_select) = match self.active_settings() {
            Some(s) => (
                s.font_select.clone(),
                s.font_bold_select.clone(),
                s.font_italic_select.clone(),
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
                .label("Reset")
                .ghost()
                .small()
                .on_click(cx.listener(|this, _, _w, cx| this.reset_font_size(cx))),
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
                .label("Reset")
                .ghost()
                .small()
                .on_click(cx.listener(|this, _, _w, cx| this.reset_line_height(cx))),
        );

        let font_dropdown = |state: &Entity<SelectState<SearchableVec<String>>>| {
            Select::new(state)
                .small()
                .w(px(180.))
                .h(control_h)
                .search_placeholder("Search fonts…")
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
                "Theme",
                "Pick a color theme. Each one sets its own light or dark look.",
                cx,
            ))
            .child(self.render_theme_selection(cx))
            .child(self.render_custom_themes(cx))
            .child(self.section_rule(cx))
            .child(self.render_window_section(cx))
            .child(self.section_rule(cx))
            .child(self.section_header("Typography", cx))
            .child(self.settings_row(
                "Font size",
                "Terminal text size in pixels.",
                font_size_control,
                cx,
            ))
            .child(self.settings_row(
                "Line height",
                "Row spacing as a multiple of the font size.",
                line_height_control,
                cx,
            ))
            .child(self.settings_row(
                "Font family",
                "Pick from fonts installed on your system.",
                font_family_control,
                cx,
            ))
            .child(self.settings_row(
                "Bold font",
                "Face for bold text; Default synthesizes it from the primary.",
                font_bold_control,
                cx,
            ))
            .child(self.settings_row(
                "Italic font",
                "Face for italic text; Default synthesizes it from the primary.",
                font_italic_control,
                cx,
            ))
            .child(self.settings_row(
                "Font ligatures",
                "Enable common programming ligature features for terminal text.",
                ligature_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Cursor", cx))
            .child(self.settings_row(
                "Cursor shape",
                "How the terminal cursor is drawn.",
                cursor_style_control,
                cx,
            ))
            .child(self.settings_row(
                "Cursor blink",
                "Pulse the cursor while the terminal is focused.",
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
        let overridden = config.window_opacity.is_some() || config.window_blur.is_some();
        let dim_inactive_panes = config.dim_inactive_panes;
        let theme = presets::by_id(cx, &crate::ui::theme::effective_preset_id(cx));
        let opacity = Tty7App::effective_window_opacity(cx);
        let blur = cx.global::<Config>().window_blur.unwrap_or(theme.blur);

        let opacity_control = h_flex()
            .items_center()
            .gap_3()
            .w(px(240.))
            .child(div().flex_1().child(Slider::new(&slider)))
            .child(
                div()
                    .w(px(36.))
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .child(format!("{:.0}%", opacity * 100.)),
            )
            .into_any_element();
        let blur_switch = crate::ui::theme::switch("window-blur", cx)
            .checked(blur)
            .on_click(
                cx.listener(|this, on: &bool, window, cx| this.set_window_blur(*on, window, cx)),
            )
            .into_any_element();
        let dim_switch = crate::ui::theme::switch("dim-inactive-panes", cx)
            .checked(dim_inactive_panes)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_dim_inactive_panes(*on, cx)))
            .into_any_element();

        v_flex()
            .child(self.section_header("Transparency", cx))
            .child(self.settings_row(
                "Opacity",
                "How opaque the window background is, for every theme. Below \
                 100% the desktop shows through.",
                opacity_control,
                cx,
            ))
            .child(self.settings_row(
                "Blur",
                "Blur whatever is behind a translucent window (macOS).",
                blur_switch,
                cx,
            ))
            .when(overridden, |this| {
                this.child(
                    h_flex().mt_2().child(
                        Button::new("follow-theme-window")
                            .label("Follow theme")
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reset_window_overrides(window, cx)
                            })),
                    ),
                )
            })
            .child(self.settings_row(
                "Dim inactive panes",
                "Fade unfocused panes in a split so the active one stands out.",
                dim_switch,
                cx,
            ))
            .into_any_element()
    }

    fn render_custom_themes(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self.active_settings().and_then(|s| s.theme_editor.as_ref());

        let folder_button = Button::new("open-themes-folder")
            .label("Open themes folder")
            .small()
            .on_click(cx.listener(|this, _, _w, cx| this.open_themes_folder(cx)));

        if let Some(editor) = editor {
            let seed: Vec<_> = editor
                .seed
                .iter()
                .map(|(_, label, state)| (label.clone(), state.clone()))
                .collect();
            let ansi: Vec<_> = editor
                .ansi
                .iter()
                .map(|(_, label, state)| (label.clone(), state.clone()))
                .collect();
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
                            "Change…"
                        } else {
                            "Choose…"
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
                            .label("Remove")
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
                            .w(px(36.))
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(format!("{:.0}%", readout * 100.)),
                    )
                    .into_any_element();
                self.settings_row(
                    "Image opacity",
                    "How strongly the image shows over the background color.",
                    control,
                    cx,
                )
            });

            return v_flex()
                .mt_5()
                .child(self.section_intro(
                    "Edit theme",
                    "You're editing a copy. Changes save to its file in the themes \
                     folder and apply live.",
                    cx,
                ))
                .children(
                    seed.into_iter()
                        .map(|(label, state)| self.render_theme_color_row(label, state, cx)),
                )
                .child(self.settings_row(
                    "Background image",
                    "Composited over the background color, under the text.",
                    image_control,
                    cx,
                ))
                .children(image_opacity_row)
                .child(self.section_header("ANSI colors", cx))
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
                "Custom themes",
                "Duplicate a theme to edit its colors here, or drop your own in the \
                 themes folder: a tty7 YAML theme or an iTerm2 .itermcolors scheme.",
                cx,
            ))
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Button::new("duplicate-theme")
                            .label("Duplicate to edit")
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
        h_flex()
            .size_full()
            .items_start()
            .child(
                v_flex()
                    .id("ssh-master")
                    .flex_shrink_0()
                    .w(px(280.))
                    .h_full()
                    .border_r_1()
                    .border_color(border)
                    .overflow_y_scroll()
                    .child(self.render_ssh_master(cx)),
            )
            .child(
                v_flex()
                    .id("ssh-detail")
                    .flex_1()
                    .h_full()
                    .overflow_y_scroll()
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
                    ),
            )
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

        let header = v_flex().gap_2().child(self.header_text("Hosts", cx)).child(
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
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_new_profile(window, cx)
                                })),
                        )
                        .child(
                            Button::new("ssh-profiles-more")
                                .icon(Icon::empty().path("stock/icons/ellipsis.svg"))
                                .ghost()
                                .small()
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
            "Defaults",
            "Inherited by every host",
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
                    .text_sm()
                    .text_color(muted)
                    .child("No saved hosts yet."),
            );
        } else if groups.is_empty() {
            list = list.child(
                div()
                    .py_4()
                    .text_sm()
                    .text_color(muted)
                    .child(format!("Nothing matches {query}.")),
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
            .item(PopupMenuItem::new("Import from ~/.ssh/config").on_click({
                let app = app.clone();
                move |_, _window, cx| {
                    let _ = app.update(cx, |this, cx| this.import_ssh_config_profiles(cx));
                }
            }))
            .item(PopupMenuItem::new("Expand all groups").on_click({
                let app = app.clone();
                move |_, _window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        if let Some(s) = this.active_settings_mut() {
                            s.ssh_collapsed_groups.clear();
                        }
                        cx.notify();
                    });
                }
            }))
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
            .px_1p5()
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
            .when_some(dot, |row, live| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .size(px(6.))
                        .rounded_full()
                        .when(live, |d| d.bg(success))
                        .when(!live, |d| d.border_1().border_color(border)),
                )
            })
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
            "No hosts yet"
        } else {
            "Nothing selected"
        };

        let mut body = v_flex()
            .gap_1()
            .child(self.header_text(heading, cx))
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child("Type an address to connect now — tty7 offers to save it afterwards."),
            )
            .child(
                h_flex()
                    .mt_3()
                    .gap_2()
                    .child(div().w(px(320.)).child(Input::new(&input).small()))
                    .child(
                        Button::new("ssh-quick-connect")
                            .label("Connect")
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
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(format!("{n} more in ~/.ssh/config")),
                            )
                            .child(div().text_xs().text_color(muted).truncate().child(names)),
                    )
                    .child(
                        Button::new("ssh-empty-import")
                            .label("Link")
                            .small()
                            .on_click(
                                cx.listener(|this, _, _w, cx| this.import_ssh_config_profiles(cx)),
                            ),
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
                match imported {
                    0 => "No aliases linked yet.".to_string(),
                    1 => "1 alias linked.".to_string(),
                    n => format!("{n} aliases linked."),
                },
                cx,
            ))
            .child(
                self.settings_row(
                    "Import aliases",
                    "Re-reads the file and adds anything new. Edits you make here are \
                 stored by tty7 — the file itself is never written.",
                    Button::new("ssh-defaults-import")
                        .label("Import now")
                        .small()
                        .on_click(
                            cx.listener(|this, _, _w, cx| this.import_ssh_config_profiles(cx)),
                        )
                        .into_any_element(),
                    cx,
                ),
            );

        v_flex()
            .child(
                v_flex()
                    .gap_1()
                    .mb_6()
                    .child(self.header_text("Defaults", cx))
                    .child(div().text_sm().text_color(muted).child(
                        "Every host starts from these. Any host can override one under \
                         its own Advanced.",
                    )),
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
            .item(PopupMenuItem::new("Connect").on_click({
                let app = app.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| {
                        this.close_settings(window, cx);
                        this.connect_ssh_profile(id, window, cx);
                    });
                }
            }))
            .item(PopupMenuItem::new("Copy address").on_click({
                let app = app.clone();
                move |_, _window, cx| {
                    let _ = app.update(cx, |this, cx| this.copy_profile_connect_string(id, cx));
                }
            }))
            .item(PopupMenuItem::new("Duplicate").on_click({
                let app = app.clone();
                move |_, window, cx| {
                    let _ = app.update(cx, |this, cx| this.duplicate_profile(id, window, cx));
                }
            }))
            .item(PopupMenuItem::new("Forget password").on_click({
                let app = app.clone();
                move |_, window, cx| {
                    if let Some(msg) = app
                        .update(cx, |this, cx| this.forget_profile_password(id, cx))
                        .ok()
                        .flatten()
                    {
                        window.push_notification(msg, cx);
                    }
                }
            }))
            .separator();

        menu.item(
            PopupMenuItem::element(move |_window, _cx| div().text_color(danger).child("Delete"))
                .on_click({
                    let app = app.clone();
                    move |_, _window, cx| {
                        let _ = app.update(cx, |this, cx| this.delete_profile(id, cx));
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
                "Security",
                "A host can override either of these under its own Advanced.",
                cx,
            ))
            .child(self.settings_row(
                "Verify host keys",
                "Check each server's key against known_hosts and confirm unknown or \
                 changed keys before connecting. Off connects without checking, so a \
                 spoofed server would go unnoticed.",
                verify_switch,
                cx,
            ))
            .child(self.settings_row(
                "Warn before closing",
                "Ask for confirmation before closing a tab or pane with a live SSH \
                 session.",
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

    fn ssh_form_collect(&self, cx: &App) -> Option<SshProfile> {
        let form = self.active_settings()?.ssh_form.as_ref()?;
        let id = form.editing;
        let val = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();

        let jump_name = val(&form.jump);
        let jump_host = if jump_name.is_empty() {
            None
        } else {
            cx.global::<Config>()
                .ssh_profiles
                .iter()
                .find(|p| p.name == jump_name && p.id != id)
                .map(|p| p.id)
        };

        Some(SshProfile {
            id,
            name: val(&form.name),
            group: form.carry_group.clone(),
            host: val(&form.host),
            port: val(&form.port).parse().unwrap_or(22),
            user: val(&form.user),
            jump_host,
            proxy_command: (!val(&form.proxy_command).is_empty()).then(|| val(&form.proxy_command)),
            socks_proxy: parse_host_port(&val(&form.socks)),
            http_proxy: parse_host_port(&val(&form.http)),
            auth: form.auth,
            identity_files: split_lines(&form.identity_files.read(cx).value()),
            agent_forward: form.agent_forward,
            credential_ref: form.carry_credential_ref.clone(),
            forwards: form.forwards.iter().filter_map(|r| r.collect(cx)).collect(),
            keepalive_interval_s: val(&form.keepalive_interval).parse().ok(),
            keepalive_count_max: val(&form.keepalive_count).parse().ok(),
            connect_timeout_s: val(&form.connect_timeout).parse().ok(),
            warn_on_close: form.warn_on_close,
            skip_banner: form.skip_banner,
            shell_integration: form.shell_integration,
            login_scripts: split_lines(&form.login_scripts.read(cx).value()),
            x11: form.x11,
            algorithms: Algorithms {
                kex: split_list(&form.kex.read(cx).value()),
                cipher: split_list(&form.cipher.read(cx).value()),
                mac: split_list(&form.mac.read(cx).value()),
                hostkey: split_list(&form.hostkey.read(cx).value()),
                compression: split_list(&form.compression.read(cx).value()),
            },
            verify_host_keys: form.verify_host_keys,
        })
    }

    pub(crate) fn save_editing_profile(&mut self, cx: &mut Context<Self>) -> Option<Uuid> {
        let profile = self.ssh_form_collect(cx)?;
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
        profile.name = format!("{} (copy)", profile.name);
        self.update_config(cx, |cfg| cfg.ssh_profiles.push(profile.clone()));
        self.ssh_form_load(&profile, window, cx);
    }

    pub(crate) fn delete_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
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

    pub(crate) fn import_ssh_config_profiles(&mut self, cx: &mut Context<Self>) {
        let imported = crate::core::ssh_config::import_profiles();
        if imported.is_empty() {
            return;
        }
        self.update_config(cx, |cfg| {
            crate::core::ssh_config::merge_imported(&mut cfg.ssh_profiles, imported);
        });
        cx.notify();
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
                Ok(()) => format!("Forgot saved password for {endpoint}"),
                Err(e) => format!("Couldn't forget password for {endpoint}: {e}"),
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
        let collected = self.ssh_form_collect(cx);
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
            (true, true) => "New host".to_string(),
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
                                r.child(div().child(format!("· via {jump_name}")))
                            })
                            .when(live, |r| {
                                r.child(div().text_color(success).child("· connected"))
                            }),
                    ),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_2()
                    .child(
                        Button::new("ssh-form-save")
                            .label("Save")
                            .small()
                            .disabled(!dirty)
                            .on_click(cx.listener(|this, _, _w, cx| this.save_ssh_form(cx))),
                    )
                    .child(
                        Button::new("ssh-form-connect")
                            .label("Connect")
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_and_connect_profile(window, cx)
                            })),
                    ),
            );

        let core = v_flex()
            .gap_3()
            .child(
                self.settings_row(
                    "Name",
                    "A label for this connection.",
                    div()
                        .w(px(260.))
                        .child(Input::new(&form.name).small())
                        .into_any_element(),
                    cx,
                ),
            )
            .child(
                self.settings_row(
                    "Host",
                    "Hostname or IP address.",
                    h_flex()
                        .gap_2()
                        .child(div().w(px(172.)).child(Input::new(&form.host).small()))
                        .child(div().w(px(80.)).child(Input::new(&form.port).small()))
                        .into_any_element(),
                    cx,
                ),
            )
            .child(
                self.settings_row(
                    "User",
                    "Login user (blank = resolve at connect).",
                    div()
                        .w(px(260.))
                        .child(Input::new(&form.user).small())
                        .into_any_element(),
                    cx,
                ),
            )
            .child(self.settings_row(
                "Auth",
                "Authentication method. Auto tries every applicable method.",
                self.segmented(
                    "ssh-form-auth",
                    &["Auto", "GSSAPI", "Password", "Key", "Agent", "2FA"],
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
            .child(self.render_ssh_profile_jump_section(form, cx))
            .child(self.render_ssh_profile_forwards_section(form, cx))
            .child(self.render_ssh_profile_advanced_section(form, cx))
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
        let caret = if open { "▾" } else { "▸" };
        h_flex()
            .id(id)
            .items_center()
            .gap_2()
            .py_2()
            .cursor_pointer()
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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let summary = {
            let name = form.jump.read(cx).value().trim().to_string();
            if name.is_empty() {
                "(none)".to_string()
            } else {
                name
            }
        };
        let mut section = v_flex().child(self.disclosure_header(
            "ssh-sec-jump",
            "Jump host",
            &summary,
            form.show_jump,
            cx,
            |this, cx| {
                if let Some(f) = this.ssh_form_mut() {
                    f.show_jump = !f.show_jump;
                    cx.notify();
                }
            },
        ));
        if form.show_jump {
            section = section.child(
                self.settings_row(
                    "Jump host",
                    "Name of another profile to tunnel through (blank = direct).",
                    div()
                        .w(px(260.))
                        .child(Input::new(&form.jump).small())
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
            0 => "none".to_string(),
            1 => "1 rule, opened with the connection".to_string(),
            n => format!("{n} rules, opened with the connection"),
        };
        let mut section = v_flex().child(self.disclosure_header(
            "ssh-sec-fwd",
            "Port forwarding",
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
                        .label("+ Add rule")
                        .ghost()
                        .small()
                        .on_click(
                            cx.listener(|this, _, window, cx| this.add_forward_rule(window, cx)),
                        ),
                ),
            )
            .child(
                h_flex()
                    .gap_3()
                    .pt_1()
                    .text_xs()
                    .text_color(muted)
                    .child("L — a local port reaches the remote side")
                    .child("R — a remote port reaches this machine")
                    .child("D — dynamic SOCKS proxy"),
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
        let danger = cx.theme().danger;
        let needs_target = row.kind != ForwardKind::Dynamic;
        let kind_idx = match row.kind {
            ForwardKind::Local => 0,
            ForwardKind::Remote => 1,
            ForwardKind::Dynamic => 2,
        };
        let incomplete = row.collect(cx).is_none() && !row.is_blank(cx);

        let endpoint = |host: &Entity<InputState>, port: &Entity<InputState>| {
            h_flex()
                .gap_1()
                .items_center()
                .child(div().w(px(104.)).child(Input::new(host).xsmall()))
                .child(div().text_xs().text_color(muted).child(":"))
                .child(div().w(px(58.)).child(Input::new(port).xsmall()))
        };

        v_flex()
            .gap_0p5()
            .py_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(self.segmented(
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
                    ))
                    .child(endpoint(&row.bind_host, &row.bind_port))
                    .child(div().text_xs().text_color(muted).child("→"))
                    .child(
                        div()
                            .opacity(if needs_target { 1.0 } else { 0.35 })
                            .child(endpoint(&row.target_host, &row.target_port)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(80.))
                            .child(Input::new(&row.description).xsmall()),
                    )
                    .child(
                        Button::new(("ssh-fwd-remove", idx))
                            .icon(Icon::new(IconName::Close))
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                this.remove_forward_rule(idx, cx)
                            })),
                    ),
            )
            .when(incomplete, |col| {
                col.child(div().text_xs().text_color(danger).child(if needs_target {
                    "Needs a listen port and a target host:port — won't be saved."
                } else {
                    "Needs a listen port — won't be saved."
                }))
            })
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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut section = v_flex().child(self.disclosure_header(
            "ssh-sec-adv",
            "Advanced",
            "algorithms / keepalive / proxies / X11 / login scripts",
            form.show_advanced,
            cx,
            |this, cx| {
                if let Some(f) = this.ssh_form_mut() {
                    f.show_advanced = !f.show_advanced;
                    cx.notify();
                }
            },
        ));
        if !form.show_advanced {
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
                    .child(Input::new(input).small())
                    .into_any_element(),
                cx,
            )
        };

        let on_off = |b: bool| if b { "on" } else { "off" };
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
            .child(text_row(
                self,
                "Identity files",
                "Private-key paths, one per line (%h/%r expand).",
                &form.identity_files,
                cx,
            ))
            .child(
                self.settings_row(
                    "Agent forwarding",
                    "Forward the local ssh-agent to the connection.",
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
            .child(text_row(
                self,
                "ProxyCommand",
                "Transport command (%h/%p/%r substituted).",
                &form.proxy_command,
                cx,
            ))
            .child(text_row(
                self,
                "SOCKS5 proxy",
                "host:port (blank = none).",
                &form.socks,
                cx,
            ))
            .child(text_row(
                self,
                "HTTP proxy",
                "host:port (blank = none).",
                &form.http,
                cx,
            ))
            .child(text_row(
                self,
                "KEX algorithms",
                "Comma-separated (blank = library default).",
                &form.kex,
                cx,
            ))
            .child(text_row(
                self,
                "Ciphers",
                "Comma-separated (blank = default).",
                &form.cipher,
                cx,
            ))
            .child(text_row(
                self,
                "MACs",
                "Comma-separated (blank = default).",
                &form.mac,
                cx,
            ))
            .child(text_row(
                self,
                "Host-key algorithms",
                "Comma-separated (blank = default).",
                &form.hostkey,
                cx,
            ))
            .child(text_row(
                self,
                "Compression",
                "Comma-separated (blank = default).",
                &form.compression,
                cx,
            ))
            .child(text_row(
                self,
                "Keepalive interval (s)",
                "Blank = library default.",
                &form.keepalive_interval,
                cx,
            ))
            .child(text_row(
                self,
                "Keepalive count max",
                "Missed keepalives before dead.",
                &form.keepalive_count,
                cx,
            ))
            .child(text_row(
                self,
                "Connect timeout (s)",
                "Blank = library default.",
                &form.connect_timeout,
                cx,
            ))
            .child(
                self.settings_row(
                    "X11 forwarding",
                    "Request X11 forwarding (needs XQuartz on macOS).",
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
                    "Shell integration",
                    "Let the remote shell report prompts, exit codes and directory.",
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
                "Login scripts",
                "Commands sent after the shell opens, one per line.",
                &form.login_scripts,
                cx,
            ))
            .child(
                self.settings_row(
                    "Skip banner",
                    "Suppress the server login banner.",
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
            .child(self.settings_row(
                "Verify host keys",
                format!("Default follows Defaults, which is {vhk_default}."),
                self.segmented(
                    "ssh-form-vhk",
                    &["Default", "On", "Off"],
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
                "Warn before closing",
                format!("Default follows Defaults, which is {woc_default}."),
                self.segmented(
                    "ssh-form-woc",
                    &["Default", "On", "Off"],
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
            "your login shell"
        };

        let program_control = div()
            .w(px(260.))
            .child(Input::new(&program_input).small())
            .into_any_element();
        let args_control = div()
            .w(px(260.))
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
            &["Inherit", "Home", "Custom"],
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
                .child(Input::new(&wd_path_input).small())
                .into_any_element()
        } else {
            div().into_any_element()
        };

        v_flex()
            .child(self.section_intro(
                "Shell",
                format!(
                    "The program each new terminal launches. Leave Program empty to use the platform default ({platform_default})."
                ),
                cx,
            ))
            .child(self.settings_row(
                "Program",
                "Executable name on PATH or an absolute path. e.g. zsh, fish, nu, pwsh.",
                program_control,
                cx,
            ))
            .child(self.settings_row(
                "Arguments",
                "Space-separated launch flags. e.g. -l for a login shell.",
                args_control,
                cx,
            ))
            .child(self.settings_row(
                "Start in",
                "What a fresh shell starts in: tty7's launch directory, your home folder, or a fixed path.",
                wd_radio,
                cx,
            ))
            .when(wd_strategy == crate::core::config::WdStrategy::Custom, |v| {
                v.child(self.settings_row(
                    "Custom path",
                    "The directory new shells start in.",
                    wd_path_control,
                    cx,
                ))
            })
            .child(
                div()
                    .mt_3()
                    .text_xs()
                    .text_color(muted_fg)
                    .child("Applies to shells with nothing to inherit — like the first tab of a window. New tabs and splits keep inheriting the active pane's directory, and shells already open keep running."),
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
        };
        let bell_control = self.segmented(
            "term-bell",
            &["Off", "Visual", "Audible"],
            bell_idx,
            cx,
            |this, ix, _w, cx| {
                let mode = match ix {
                    0 => BellMode::None,
                    1 => BellMode::Visual,
                    _ => BellMode::Audible,
                };
                this.set_bell_mode(mode, cx);
            },
        );
        let scroll_control = h_flex()
            .items_center()
            .gap_3()
            .w(px(240.))
            .child(div().flex_1().child(Slider::new(&scroll_slider)))
            .child(
                div()
                    .w(px(36.))
                    .text_sm()
                    .text_color(foreground)
                    .child(format!("{scroll_mult:.2}×")),
            )
            .into_any_element();

        v_flex()
            .child(self.render_shell_group(cx))
            .child(self.section_rule(cx))
            .child(self.section_header("Scrolling", cx))
            .child(self.settings_row(
                "Scrollback",
                "Lines of history kept per pane. Applies to new panes.",
                scrollback_radio,
                cx,
            ))
            .child(self.settings_row(
                "Scroll speed",
                "Multiplier applied to mouse-wheel scrolling.",
                scroll_control,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Mouse", cx))
            .child(self.settings_row(
                "Focus follows mouse",
                "Hovering a pane focuses it without a click.",
                focus_switch,
                cx,
            ))
            .child(self.settings_row(
                "Hide mouse while typing",
                "Hide the pointer as you type; it returns on the next move.",
                mouse_hide_switch,
                cx,
            ))
            .child(self.settings_row(
                "Report mouse to apps",
                "Let full-screen apps (vim, tmux) handle clicks and scrolling; hold Shift to keep a gesture local.",
                mouse_report_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Bell", cx))
            .child(self.settings_row(
                "Terminal bell",
                "How a bell (^G) is signalled: silenced, a brief flash, or the system sound.",
                bell_control,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Links", cx))
            .child(self.settings_row(
                "Detect URLs",
                format!("Underline links on hover and open them on {LINK_MODIFIER_LABEL}-click."),
                link_switch,
                cx,
            ))
            .child(self.settings_row(
                "Forward SSH loopback links",
                "When a pane is in SSH, open localhost links through a temporary port forward.",
                ssh_loopback_switch,
                cx,
            ))
            .child(self.settings_row(
                "Open files with",
                format!(
                    "Command run when {LINK_MODIFIER_LABEL}-clicking a file link, instead of \
                     the default app. Use {{path}}, {{line}}, {{column}}; a flag whose value \
                     is absent is dropped (e.g. herdr edit {{path}} --line={{line}}). Empty \
                     uses the default app."
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
                "Option (⌥) acts as Meta",
                "⌥+key sends the escape chord shells expect (⌥B = back one word) \
                 instead of typing a special character (∫).",
                switch,
                cx,
            )
        });

        v_flex()
            .child(self.section_intro(
                "Prompt",
                "tty7's own menus at the shell prompt. Turn one off to hand the key back to the shell.",
                cx,
            ))
            .child(self.settings_row(
                "Tab completion",
                "Tab at the prompt opens tty7's completion menu. When off, Tab goes to the \
                 shell's own completion instead.",
                tab_completion_switch,
                cx,
            ))
            .child(self.settings_row(
                "History search",
                "⌃R at the prompt opens tty7's fuzzy history menu. When off, ⌃R goes to the \
                 shell instead — its own reverse-i-search, or whatever you've bound there \
                 (fzf, percol).",
                history_search_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Selection & clipboard", cx))
            .child(self.settings_row(
                "Smart selection",
                "Double-click selects the whole URL, file path, email, or bracket pair under the cursor.",
                smart_select_switch,
                cx,
            ))
            .child(self.settings_row(
                "Copy on select",
                "Selecting text with the mouse copies it to the clipboard right away, no ⌘C needed.",
                copy_on_select_switch,
                cx,
            ))
            .child(self.settings_row(
                "Trim trailing spaces on copy",
                "Strip trailing whitespace from each copied line.",
                trim_switch,
                cx,
            ))
            .when_some(option_alt_row, |v, row| {
                v.child(self.section_rule(cx))
                    .child(self.section_header("Keyboard", cx))
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
        let mut page = v_flex().child(self.section_intro(
            "Agents",
            "Hook integrations give panes running these agents live session status \
             (working / waiting / done) in the tab bar. Only active inside tty7.",
            cx,
        ));

        page = page.children(self.agent_hooks_machine_picker(selected_host, cx));

        match view {
            AgentHooksView::Loading => {
                return page
                    .child(
                        div()
                            .py_4()
                            .text_sm()
                            .text_color(muted_fg)
                            .child("Reading this machine's agent config…"),
                    )
                    .into_any_element();
            }
            AgentHooksView::Unavailable(reason) => {
                return page
                    .child(div().py_4().text_sm().text_color(warning).child(reason))
                    .into_any_element();
            }
            AgentHooksView::Ready(rows) => {
                for (i, row) in rows.into_iter().enumerate() {
                    let agent = row.agent;
                    let (dot_color, status_text) = match row.state {
                        HooksState::NotInstalled => (muted_fg, "Not installed"),
                        HooksState::Installed => (success, "Installed"),
                        HooksState::Outdated => (warning, "Outdated"),
                    };
                    let primary_label = match row.state {
                        HooksState::NotInstalled => "Install",
                        HooksState::Installed => "Reinstall",
                        HooksState::Outdated => "Update",
                    };
                    let row_note = note
                        .as_ref()
                        .filter(|(for_agent, _)| *for_agent == agent)
                        .map(|(_, text)| text.clone());

                    let control = v_flex()
                        .gap_2()
                        .items_end()
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
                                            .label("Uninstall")
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
                                    .text_xs()
                                    .text_right()
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
        page.into_any_element()
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
                    col.child(div().text_xs().text_color(muted_fg).child(format!(
                        "{offline} more saved machine{} not connected — open a workspace on one to \
                     install its hooks there.",
                        if offline == 1 { " is" } else { "s are" }
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
        let confirm_window_close = cfg.confirm_window_close;
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
            &["Never", "When Unfocused", "Always"],
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
        let confirm_close_switch = crate::ui::theme::switch("wt-confirm-window-close", cx)
            .checked(confirm_window_close)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_confirm_window_close(*on, cx)))
            .into_any_element();
        let tray_switch = crate::ui::theme::switch("wt-tray-icon", cx)
            .checked(show_tray_icon)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_show_tray_icon(*on, cx)))
            .into_any_element();
        let startup_radio = self.segmented(
            "wt-startup",
            &["Normal", "Maximized", "Fullscreen"],
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
            &["After current", "At end"],
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
            &["Top", "Left"],
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
            &["By repo", "Flat"],
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
            .child(self.section_header("Window", cx))
            .child(self.settings_row(
                "Startup window",
                "Window state when tty7 launches.",
                startup_radio,
                cx,
            ))
            .child(self.settings_row(
                "Remember window size & position",
                "Reopen at the size and position the window had when tty7 last quit. Off opens centered at the default size.",
                remember_window_switch,
                cx,
            ))
            .child(self.settings_row(
                "Restore last layout",
                "Reopen the last window's tabs, splits, and directories on launch. Off starts with a single fresh terminal.",
                restore_switch,
                cx,
            ))
            .child(self.settings_row(
                "Confirm before closing the last window",
                "Ask first, since that close also quits tty7. Off closes straight away — \
                 either way your shells keep running in the background.",
                confirm_close_switch,
                cx,
            ))
            .child(self.settings_row(
                "Show tray icon",
                "Keep a status item in the system tray / menu bar: it signals when a \
                 coding agent needs your input, and its menu jumps to agent panes.",
                tray_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Tabs", cx))
            .child(self.settings_row(
                "New tab position",
                "Where a freshly opened tab is inserted.",
                new_tab_radio,
                cx,
            ))
            .child(self.settings_row(
                "Tab bar position",
                "Show tabs as a horizontal strip on top or a vertical sidebar on the left.",
                tab_bar_radio,
                cx,
            ))
            .child(self.settings_row(
                "Sidebar grouping",
                "Group sidebar tabs under a header per git repository, with non-repo tabs \
                 in a Scratch section. Only applies to the left sidebar.",
                sidebar_grouping_radio,
                cx,
            ))
            .child(self.settings_row(
                "Open diff preview from sidebar counts",
                "Click a row's +N −N to open the working-tree diff in an overlay. Off keeps the \
                 branch and the counts on the row and just stops them being clickable.",
                sidebar_diff_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Notifications", cx))
            .child(self.settings_row(
                "Notify on command finish",
                "Desktop alert after a long foreground command completes.",
                notify_radio,
                cx,
            ))
            .child(self.settings_row(
                "Notify threshold",
                "How long a command must run to qualify as \"long\".",
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
            .rounded(px(8.))
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
        let root = v_flex().child(self.settings_row(
            "Sync with system",
            "Follow the OS appearance with separate light and dark themes.",
            follow_switch,
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
            "Custom"
        } else {
            "Built-in"
        };
        let caption = match slot {
            ThemeSlot::Manual => {
                let mode = if active.dark { "Dark" } else { "Light" };
                format!("{kind} · {mode}")
            }
            ThemeSlot::Light if !crate::ui::theme::system_dark(cx) => {
                format!("Light mode · {kind} · Active")
            }
            ThemeSlot::Light => format!("Light mode · {kind}"),
            ThemeSlot::Dark if crate::ui::theme::system_dark(cx) => {
                format!("Dark mode · {kind} · Active")
            }
            ThemeSlot::Dark => format!("Dark mode · {kind}"),
        };
        let to_u32 = |(r, g, b): (u8, u8, u8)| (r as u32) << 16 | (g as u32) << 8 | b as u32;
        let swatches = h_flex().gap_1().mt_1p5().children((1..=6).map(|i| {
            div()
                .w(px(10.))
                .h(px(10.))
                .rounded(px(3.))
                .bg(rgb(to_u32(active.ansi16[i])))
        }));
        let preview = self.theme_preview(&active);
        let open = self
            .active_settings()
            .is_some_and(|s| s.theme_panel_open && s.theme_panel_slot == slot);

        div()
            .id(card_id)
            .mt_1()
            .mb_2()
            .w_full()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _w, cx| this.toggle_theme_panel(slot, cx)))
            .child(
                h_flex()
                    .items_center()
                    .gap_4()
                    .p_3()
                    .rounded_xl()
                    .border_1()
                    .border_color(if open {
                        foreground.opacity(0.35)
                    } else {
                        border
                    })
                    .bg(surface)
                    .hover(|h| h.bg(hover_bg))
                    .child(div().w(px(150.)).flex_shrink_0().child(preview))
                    .child(
                        v_flex()
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
                    .child(div().flex_1())
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .text_sm()
                            .text_color(muted_fg)
                            .child("Change theme")
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
                    .child("Themes"),
            )
            .child(
                div().occlude().child(
                    Button::new("theme-panel-close")
                        .icon(IconName::Close)
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _w, cx| this.close_theme_panel(cx))),
                ),
            );

        let subtitle = div()
            .px_4()
            .pb_3()
            .text_xs()
            .text_color(muted_fg)
            .child(match slot {
                ThemeSlot::Manual => "Change your current theme.",
                ThemeSlot::Light => "Choose the theme for light mode.",
                ThemeSlot::Dark => "Choose the theme for dark mode.",
            });

        let search_box = div().px_4().pb_3().child(
            div().w(px(268.)).child(
                Input::new(&search).small().prefix(
                    Icon::empty()
                        .path("stock/icons/search.svg")
                        .small()
                        .text_color(muted_fg),
                ),
            ),
        );

        let mut list = v_flex().px_4().pb_4().gap_4();
        for p in presets::all(cx) {
            if !query.is_empty() && !p.name.to_lowercase().contains(&query) {
                continue;
            }
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
                            .w(px(268.))
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
                            .child(
                                div()
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
                                s.child(Icon::new(IconName::Check).small().text_color(foreground))
                            }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| match slot {
                        ThemeSlot::Manual => this.set_preset(&click_id, window, cx),
                        ThemeSlot::Light => this.set_slot_preset(false, &click_id, window, cx),
                        ThemeSlot::Dark => this.set_slot_preset(true, &click_id, window, cx),
                    })),
            );
        }

        v_flex()
            .w(px(300.))
            .h_full()
            .flex_shrink_0()
            .bg(bg)
            .border_l_1()
            .border_color(border)
            .child(header)
            .child(subtitle)
            .child(search_box)
            .child(
                v_flex()
                    .id("theme-panel-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(list),
            )
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
            &["Default", "tmux"],
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

        let preset_row = h_flex()
            .items_center()
            .justify_between()
            .py_2()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(foreground)
                            .child("Preset"),
                    )
                    .child(div().text_xs().text_color(muted).child(
                        "tmux remaps pane/tab actions onto prefix sequences (e.g. Ctrl-B then C).",
                    )),
            )
            .child(h_flex().flex_shrink_0().child(preset_control));

        let prefix_row = h_flex()
            .items_center()
            .justify_between()
            .py_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(foreground)
                    .child("Prefix"),
            )
            .child(h_flex().flex_shrink_0().child(prefix_control));

        let count = effective.len();
        let mut list = v_flex().mt_2();
        for (i, (action, key)) in effective.into_iter().enumerate() {
            let is_recording = recording.as_ref().is_some_and(|(a, _)| a == &action);
            let is_overridden = overridden.contains(&action);

            let keycaps = |spec: &str| {
                h_flex().gap_2().children(
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
                    row.child(div().text_xs().text_color(accent).child("Press keys…"))
                } else {
                    row.child(keycaps(&chords.join(" "))).child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("pause to save · Esc"),
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
                            .label("Reset")
                            .small()
                            .on_click(cx.listener(move |this, _, _w, cx| {
                                this.reset_keybinding(action_for_reset.clone(), cx)
                            })),
                    )
                });

            list = list.child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .py_1p5()
                    .when(i + 1 < count, |s| s.border_b_1().border_color(border))
                    .child(
                        div()
                            .text_sm()
                            .text_color(foreground)
                            .child(humanize_action(&action)),
                    )
                    .child(right),
            );
        }

        v_flex()
            .child(self.section_intro(
                "Keybindings",
                "Click a shortcut, then press the new keys — it saves after a brief pause. Chain keys for a sequence like Ctrl-B then X. Esc cancels; Backspace removes the last key, or resets the shortcut to default when pressed first.",
                cx,
            ))
            .child(preset_row)
            .when(tmux, |v| v.child(prefix_row))
            .when(tmux, |v| {
                v.child(div().py_1().text_xs().text_color(muted).child(
                    "With a prefix active, a bare prefix key reaches the shell after a ~1s pause, and prefix + an unbound key is sent through to the terminal.",
                ))
            })
            .when_some(note, |v, note| {
                v.child(div().py_1().text_xs().text_color(accent).child(note))
            })
            .child(
                h_flex().justify_end().py_2().child(
                    Button::new("kb-restore-all")
                        .label("Restore all defaults")
                        .small()
                        .on_click(cx.listener(|this, _, _w, cx| {
                            this.restore_default_keybindings(cx)
                        })),
                ),
            )
            .child(list)
            .into_any_element()
    }

    fn render_settings_about(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (foreground, muted_fg, success, warning) = (
            theme.foreground,
            theme.muted_foreground,
            theme.success,
            theme.warning,
        );

        let update_status = cx
            .try_global::<crate::core::update::UpdateStatus>()
            .cloned()
            .unwrap_or_default();
        let update = update_status.available.clone();
        let update_busy = matches!(
            update_status.phase,
            crate::core::update::UpdatePhase::Checking
                | crate::core::update::UpdatePhase::Downloading
                | crate::core::update::UpdatePhase::Installing
        );
        let phase_text = match &update_status.phase {
            crate::core::update::UpdatePhase::Idle => None,
            crate::core::update::UpdatePhase::Checking => Some("Checking for updates…".to_string()),
            crate::core::update::UpdatePhase::UpToDate => {
                Some("You're running the latest version.".to_string())
            }
            crate::core::update::UpdatePhase::Downloading => {
                Some("Downloading and verifying the update…".to_string())
            }
            crate::core::update::UpdatePhase::Installing => {
                Some("Relaunching with the update…".to_string())
            }
            crate::core::update::UpdatePhase::Failed(message) => Some(message.clone()),
        };
        let check_for_updates = cx.global::<Config>().check_for_updates;
        let install_cli_on_path = cx.global::<Config>().install_cli_on_path;
        let (explorer_status, explorer_note) = self
            .active_settings()
            .map(|settings| {
                (
                    settings.explorer_context_menu_status.clone(),
                    settings.explorer_context_menu_note.clone(),
                )
            })
            .unwrap_or((
                Ok(crate::core::explorer_context_menu::Status::Unsupported),
                None,
            ));
        let (
            explorer_status_text,
            explorer_status_color,
            register_label,
            register_disabled,
            unregister_disabled,
        ) = match explorer_status.as_ref() {
            Ok(crate::core::explorer_context_menu::Status::NotRegistered) => {
                ("Not registered", muted_fg, "Register", false, true)
            }
            Ok(crate::core::explorer_context_menu::Status::Registered) => {
                ("Registered", success, "Register", true, false)
            }
            Ok(crate::core::explorer_context_menu::Status::NeedsUpdate) => {
                ("Needs update", warning, "Update", false, false)
            }
            Ok(crate::core::explorer_context_menu::Status::Unsupported) => {
                ("Unavailable", muted_fg, "Register", true, true)
            }
            Err(_) => ("Status unavailable", warning, "Register", false, false),
        };
        let explorer_feedback = explorer_note.or_else(|| explorer_status.err());

        let logo = Arc::new(Image::from_bytes(
            ImageFormat::Png,
            include_bytes!("../../assets/logo@256.png").to_vec(),
        ));

        v_flex()
            .child(self.section_header("About", cx))
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
                                "Version {}",
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
                v_flex()
                    .mt_5()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(foreground)
                            .child("A terminal workbench: shells, workspaces, SSH, coding agents."),
                    )
                    .child(div().text_sm().text_color(muted_fg).child(
                        "Editor-grade input in every shell, shells that outlive quits and reboots without tmux, a native SSH stack with profiles and port forwarding, and live status for panes running coding agents.",
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child("Pure Rust · GPU rendering on Zed's gpui · VT core from Alacritty"),
                    ),
            )
            .child(
                v_flex()
                    .mt_6()
                    .gap_2()
                    .child(self.section_rule(cx))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(foreground)
                            .child("Updates"),
                    )
                    .when_some(update, |this, upd| {
                        let button_label = if upd.installable {
                            "Update and Relaunch"
                        } else {
                            "View Release"
                        };
                        this.child(
                            v_flex()
                                .gap_1()
                                .child(
                                    h_flex()
                                        .gap_3()
                                        .items_center()
                                        .child(div().text_sm().text_color(foreground).child(
                                            format!("Version {} is available.", upd.version),
                                        ))
                                        .child(
                                            Button::new("install-update")
                                                .label(button_label)
                                                .small()
                                                .disabled(update_busy)
                                                .on_click(cx.listener(|_, _, _window, cx| {
                                                    crate::core::update::install_available(cx)
                                                })),
                                        ),
                                )
                                .when_some(upd.install_hint, |this, hint| {
                                    this.child(div().text_xs().text_color(muted_fg).child(hint))
                                }),
                        )
                    })
                    .when_some(phase_text, |this, text| {
                        this.child(div().text_sm().text_color(muted_fg).child(text))
                    })
                    .child(div().text_sm().text_color(muted_fg).child(
                        "tty7 checks stable releases and can update packaged macOS app bundles without opening a browser. A dedicated helper verifies checksums, version, and code signing before replacement, then relaunches the GUI. Compatible servers and shells stay running; if the wire protocol changed, tty7 asks whether to restart the server after relaunch. Other platforms and unsupported layouts fall back to the release page.",
                    ))
                    .child(
                        h_flex().child(
                            Button::new("check-update-now")
                                .label(if matches!(
                                    update_status.phase,
                                    crate::core::update::UpdatePhase::Checking
                                ) {
                                    "Checking…"
                                } else {
                                    "Check Now"
                                })
                                .small()
                                .disabled(update_busy)
                                .on_click(cx.listener(|_, _, _window, cx| {
                                    crate::core::update::spawn_check_forced(cx)
                                })),
                        ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                crate::ui::theme::switch("check-updates", cx)
                                    .checked(check_for_updates)
                                    .on_click(cx.listener(|this, on: &bool, _w, cx| {
                                        this.set_check_for_updates(*on, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(foreground)
                                    .child("Check for updates on launch"),
                            ),
                    ),
            )
            .when(cfg!(windows), |page| {
                page.child(
                    v_flex()
                        .mt_6()
                        .gap_2()
                        .child(self.section_rule(cx))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(foreground)
                                .child("Windows Explorer"),
                        )
                        .child(div().text_sm().text_color(muted_fg).child(
                            "Add “Open in tty7” when you right-click a folder and “Open tty7 here” when you right-click a folder background. This is off by default and is registered only for your Windows account.",
                        ))
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(div().size_2().rounded_full().bg(explorer_status_color))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(foreground)
                                        .child(explorer_status_text),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("explorer-menu-register")
                                        .label(register_label)
                                        .small()
                                        .disabled(register_disabled)
                                        .on_click(cx.listener(|this, _, _window, cx| {
                                            this.register_explorer_context_menu(cx)
                                        })),
                                )
                                .child(
                                    Button::new("explorer-menu-unregister")
                                        .label("Unregister")
                                        .small()
                                        .disabled(unregister_disabled)
                                        .on_click(cx.listener(|this, _, _window, cx| {
                                            this.unregister_explorer_context_menu(cx)
                                        })),
                                ),
                        )
                        .when_some(explorer_feedback, |section, message| {
                            section.child(
                                div()
                                    .text_xs()
                                    .text_color(muted_fg)
                                    .child(message),
                            )
                        })
                        .child(div().text_xs().text_color(muted_fg).child(
                            "On Windows 11, classic shell entries may appear under “Show more options”.",
                        )),
                )
            })
            .child(
                v_flex()
                    .mt_6()
                    .gap_2()
                    .child(self.section_rule(cx))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(foreground)
                            .child("Command line"),
                    )
                    .child(div().text_sm().text_color(muted_fg).child(
                        "Put the bundled `tty7` command on your PATH at launch, so scripts and coding agents can drive tty7 from any terminal. Inside a tty7 pane it works either way. Turn this off if you keep your own `tty7` — one you built or installed yourself — and do not want it shadowed. Takes effect at next launch.",
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                crate::ui::theme::switch("install-cli-on-path", cx)
                                    .checked(install_cli_on_path)
                                    .on_click(cx.listener(|this, on: &bool, _w, cx| {
                                        this.set_install_cli_on_path(*on, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(foreground)
                                    .child("Install the `tty7` command on PATH"),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .mt_6()
                    .gap_2()
                    .child(self.section_rule(cx))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(foreground)
                            .child("Server"),
                    )
                    .child(div().text_sm().text_color(muted_fg).child(
                        "Restart the server on this computer to pick up a newly granted macOS permission, recover if it stops responding, or start from a clean slate. This ends all running shells here; your tabs and layout reopen with fresh shells. A remote machine's server is restarted from its own menu in the workspace switcher.",
                    ))
                    .child(
                        h_flex().child(
                            Button::new("restart-daemon")
                                .label("Restart server…")
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
            let query = entry.title.to_lowercase();
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
        let cases: &[(&str, SettingsSection)] = &[
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
            ("right click", About),
        ];
        for (query, expected) in cases {
            assert_eq!(
                best_matching_section(query).map(|s| s.profile_label()),
                Some(expected.profile_label()),
                "query {query:?} should land on {:?}",
                expected.profile_label()
            );
        }
    }

    #[test]
    fn close_confirmation_toggle_is_findable() {
        for query in [
            "ask again",
            "closing the last window",
            "dialog",
            "cmd-w",
            "ctrl-w",
        ] {
            assert_eq!(
                best_matching_section(query).map(|s| s.profile_label()),
                Some(SettingsSection::WindowTabs.profile_label()),
                "query {query:?} should land on Window & Tabs"
            );
        }
    }

    #[test]
    fn index_titles_match_rendered_row_labels() {
        for title in [
            "Start in",
            "Restore last layout",
            "Confirm before closing the last window",
            "Terminal bell",
            "Report mouse to apps",
            "Open files with",
            "Sidebar grouping",
            "Tab completion",
            "History search",
            "Dim inactive panes",
            "Option (⌥) acts as Meta",
        ] {
            assert!(
                settings_search_entries().iter().any(|e| e.title == title),
                "no index entry titled {title:?}"
            );
        }
    }

    #[test]
    fn agent_rows_are_in_the_search_index() {
        for agent in crate::core::agent_hooks::HookAgent::ALL {
            assert!(
                settings_search_entries().iter().any(
                    |e| e.section == SettingsSection::Agents && e.title == agent.display_name()
                ),
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
        assert!(parse_host_port("  ").is_none());
        let hp = parse_host_port("example.com:2222").unwrap();
        assert_eq!(hp.host, "example.com");
        assert_eq!(hp.port, 2222);
        assert_eq!(parse_host_port("host").unwrap().port, 0);
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

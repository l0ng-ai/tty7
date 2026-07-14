//! The Settings tab UI (Cmd+,): a sidebar of sections beside a scrollable
//! content pane. This module owns the panel's *state types* and its *rendering*
//! only; the lifecycle (opening/closing the tab, committing the font family,
//! applying theme/font changes) lives in `app.rs`, where it can touch the
//! shell's tabs and panes. The render methods extend `Tty7App` from here so the
//! window shell stays focused on tab/pane orchestration.

use gpui::{
    AnyElement, App, Context, Div, Entity, FontWeight, Image, ImageFormat, KeyDownEvent,
    MouseButton, SharedString, Stateful, Subscription, Window, WindowControlArea, div, img,
    prelude::*, px, relative, rgb,
};
use gpui_component::InteractiveElementExt as _;
use gpui_component::Selectable as _;
use gpui_component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_component::color_picker::{ColorPicker, ColorPickerState};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_component::select::{SearchableVec, Select, SelectState};
use gpui_component::sidebar::{Sidebar, SidebarCollapsible, SidebarMenu, SidebarMenuItem};
use gpui_component::slider::{Slider, SliderState};
use gpui_component::switch::Switch;
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use uuid::Uuid;

use crate::core::config::{
    BellMode, Config, CursorStyle, NewTabPosition, NotifyMode, TabBarPosition,
};
use crate::core::keychain::CredentialRef;
use crate::core::ssh_profile::{
    Algorithms, AuthMode, ForwardKind, ForwardRule, HostPort, SshProfile, to_connect_string,
};
use crate::ui::app::{FONT_SIZE_STEP, LINE_HEIGHT_STEP, ThemeEdit, Tty7App};
use crate::ui::presets;

/// Which section of the settings panel is currently selected in the sidebar.
/// Sections are named for the *object* being configured (the appearance, the
/// terminal, the shell, the window) — never for a property class like
/// "Behavior", which reads fine but predicts nothing about what's inside.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    Appearance,
    Terminal,
    Shell,
    Ssh,
    WindowTabs,
    Keybindings,
    About,
}

impl SettingsSection {
    /// A `&'static` label for `TTY7_PROFILE` aggregation, so each section's build
    /// cost and rebuild rate report under their own line.
    fn profile_label(self) -> &'static str {
        match self {
            SettingsSection::Appearance => "settings:appearance",
            SettingsSection::Terminal => "settings:terminal",
            SettingsSection::Shell => "settings:shell",
            SettingsSection::Ssh => "settings:ssh",
            SettingsSection::WindowTabs => "settings:window-tabs",
            SettingsSection::Keybindings => "settings:keybindings",
            SettingsSection::About => "settings:about",
        }
    }
}

/// One searchable setting for the settings-search box: the row's display title,
/// the section it lives in, and a bag of extra keywords/synonyms so a search
/// lands even when the user's word isn't in the visible label. Matching is
/// case-insensitive substring over `title` + `keywords`.
struct SearchEntry {
    section: SettingsSection,
    title: &'static str,
    keywords: &'static str,
}

/// The static index the settings search matches against — one entry per notable
/// setting, mirroring the rows each `render_settings_*` builds. Keywords carry
/// synonyms the visible label omits (e.g. "meta" → the Option/Alt row, "color"
/// → the theme) so intent-based searches still resolve to the right section.
fn settings_search_entries() -> &'static [SearchEntry] {
    use SettingsSection::*;
    &[
        // Appearance
        SearchEntry {
            section: Appearance,
            title: "Theme",
            keywords: "appearance color colours scheme dark light palette background foreground accent",
        },
        SearchEntry {
            section: Appearance,
            title: "Font Family",
            keywords: "typeface monospace typography",
        },
        SearchEntry {
            section: Appearance,
            title: "Font Size",
            keywords: "typography text bigger smaller zoom",
        },
        SearchEntry {
            section: Appearance,
            title: "Line Height",
            keywords: "typography leading spacing",
        },
        SearchEntry {
            section: Appearance,
            title: "Bold Font",
            keywords: "typeface weight",
        },
        SearchEntry {
            section: Appearance,
            title: "Italic Font",
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
            title: "Blink cursor",
            keywords: "caret blinking flash",
        },
        SearchEntry {
            section: Appearance,
            title: "ANSI colors",
            keywords: "palette 16 terminal colours theme",
        },
        // Terminal
        SearchEntry {
            section: Terminal,
            title: "Option acts as Meta",
            keywords: "alt keyboard modifier escape macos",
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
            title: "Detect URLs",
            keywords: "links hyperlink clickable open",
        },
        SearchEntry {
            section: Terminal,
            title: "Forward SSH loopback links",
            keywords: "ssh remote port tunnel localhost forward",
        },
        SearchEntry {
            section: Terminal,
            title: "Copy on select",
            keywords: "clipboard selection yank",
        },
        SearchEntry {
            section: Terminal,
            title: "Trim trailing spaces on copy",
            keywords: "clipboard whitespace",
        },
        SearchEntry {
            section: Terminal,
            title: "Notify on command finish",
            keywords: "notification alert bell done osc",
        },
        // Shell
        SearchEntry {
            section: Shell,
            title: "Program",
            keywords: "shell binary zsh bash fish executable",
        },
        SearchEntry {
            section: Shell,
            title: "Arguments",
            keywords: "shell flags login args",
        },
        SearchEntry {
            section: Shell,
            title: "Working directory",
            keywords: "cwd start folder path directory",
        },
        // SSH
        SearchEntry {
            section: Ssh,
            title: "Verify host keys",
            keywords: "ssh security known_hosts fingerprint mitm host key verification",
        },
        // Window & Tabs
        SearchEntry {
            section: WindowTabs,
            title: "Startup window",
            keywords: "restore session launch open",
        },
        SearchEntry {
            section: WindowTabs,
            title: "New tab position",
            keywords: "tabs order end after",
        },
        SearchEntry {
            section: WindowTabs,
            title: "Tab bar position",
            keywords: "tabs vertical sidebar left top layout",
        },
        // Keybindings
        SearchEntry {
            section: Keybindings,
            title: "Keybindings",
            keywords: "shortcut hotkey keyboard binding chord tmux preset rebind",
        },
        // About
        SearchEntry {
            section: About,
            title: "About",
            keywords: "version license credits build",
        },
    ]
}

/// Does this entry match the (already lowered, trimmed) query? Matches on the
/// visible title or any of its synonym keywords, so intent-based searches land.
fn entry_matches(entry: &SearchEntry, query: &str) -> bool {
    entry.title.to_lowercase().contains(query) || entry.keywords.contains(query)
}

/// How many of `section`'s settings match `query` — the `(N)` shown beside each
/// section link while a search is active. `query` must already be lowered/trimmed.
pub(crate) fn section_match_count(section: SettingsSection, query: &str) -> usize {
    settings_search_entries()
        .iter()
        .filter(|e| e.section == section && entry_matches(e, query))
        .count()
}

/// The section a search should jump to: the one with the most matches, ties
/// broken by nav order (the first section wins). `None` when nothing matches, so
/// the caller leaves the current selection alone.
pub(crate) fn best_matching_section(query: &str) -> Option<SettingsSection> {
    use SettingsSection::*;
    [Appearance, Terminal, Shell, WindowTabs, Keybindings, About]
        .into_iter()
        .map(|s| (s, section_match_count(s, query)))
        .filter(|(_, n)| *n > 0)
        // `>` (not `>=`) so an equal later section never displaces the earlier one.
        .reduce(|best, cur| if cur.1 > best.1 { cur } else { best })
        .map(|(s, _)| s)
}

/// The in-app color editor for the active *editable* theme: one color picker per
/// seed color (background/foreground/accent/cursor/selection) and per ANSI slot,
/// each wired to write its change straight back to the theme's YAML file. Rebuilt
/// by `Tty7App::rebuild_theme_editor` whenever the active theme changes, so it
/// always targets (and reflects) the theme on screen.
pub(crate) struct ThemeEditor {
    /// The id the pickers were built for (which theme they edit).
    #[allow(dead_code)]
    pub(crate) for_id: String,
    /// Seed-color pickers: `(edit target, row label, picker state)`.
    pub(crate) seed: Vec<(ThemeEdit, String, Entity<ColorPickerState>)>,
    /// One picker per ANSI slot 0–15.
    pub(crate) ansi: Vec<(ThemeEdit, String, Entity<ColorPickerState>)>,
    pub(crate) _subs: Vec<Subscription>,
}

/// Live state for the settings panel (Cmd+,). Holds the panel's focus owner
/// (so Esc closes it), the currently selected sidebar section, and the
/// font-family text input plus its commit subscriptions.
pub(crate) struct SettingsState {
    pub(crate) focus_handle: gpui::FocusHandle,
    pub(crate) section: SettingsSection,
    /// Live query for the settings search box in the nav header. While non-empty
    /// the nav rail lists matching settings (across every section) instead of the
    /// six section links; picking one jumps to its section.
    pub(crate) search: Entity<InputState>,
    pub(crate) font_select: Entity<SelectState<SearchableVec<String>>>,
    /// Bold / italic face pickers. Their first row is the `FONT_DEFAULT_LABEL`
    /// sentinel, meaning "reuse the primary face with synthesized emphasis".
    pub(crate) font_bold_select: Entity<SelectState<SearchableVec<String>>>,
    pub(crate) font_italic_select: Entity<SelectState<SearchableVec<String>>>,
    /// Shell program override (empty = the platform default shell).
    pub(crate) shell_program_input: Entity<InputState>,
    /// Shell launch arguments, space-separated (e.g. `-l`).
    pub(crate) shell_args_input: Entity<InputState>,
    /// Custom working-directory path (used when the strategy is `Custom`).
    pub(crate) wd_path_input: Entity<InputState>,
    /// Mouse-scroll multiplier slider (Terminal section).
    pub(crate) scroll_slider: Entity<SliderState>,
    /// The color editor for the active editable theme, or `None` when the active
    /// theme is read-only (a built-in / import) or the system is being followed.
    pub(crate) theme_editor: Option<ThemeEditor>,
    /// Whether the theme picker panel is open beside the content pane
    /// (Appearance section only). Toggled from the "Current theme" card.
    pub(crate) theme_panel_open: bool,
    /// Live filter for the theme picker panel's list.
    pub(crate) theme_search: Entity<InputState>,
    /// `Some` while a Keybindings row is capturing a new shortcut: the action
    /// being rebound plus the live keystroke interceptor that swallows and
    /// records the next keypress (see `Tty7App::start_recording_key`).
    pub(crate) recording: Option<Recording>,
    /// A transient one-line note under the Keybindings header — e.g. after a
    /// captured key was already taken and its previous owner was unbound.
    /// Cleared when the next capture starts.
    pub(crate) rebinding_note: Option<String>,
    /// The SSH-profile edit form, when a profile in the SSH section is being
    /// added or edited. `None` shows just the saved-profile list. Its widgets
    /// (inputs) are built lazily when a profile is selected and rebuilt (a fresh
    /// input set) each time, so the section never carries N profiles' worth of
    /// inputs up front. See `SshProfileForm`.
    pub(crate) ssh_form: Option<SshProfileForm>,
    /// Which detail the SSH section's right (detail) pane is showing. The section
    /// is a two-column master-detail: the left column lists profiles, and this
    /// tracks the selected one. `Profile(id)` pairs with `ssh_form` (the loaded
    /// edit form); `None` shows the empty state (the "pick a profile" hint plus
    /// the two global security toggles).
    pub(crate) ssh_detail: SshDetail,
    pub(crate) _subs: Vec<Subscription>,
}

/// The SSH section's right-pane selection (see [`SettingsState::ssh_detail`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshDetail {
    /// Nothing selected — the right pane shows the empty state (the "pick a
    /// profile" hint plus the two global security toggles).
    None,
    /// A profile's edit form (paired with `ssh_form`, keyed by the profile id).
    Profile(Uuid),
}

/// The live edit-form state for one SSH profile, folded into Settings → SSH.
/// A single reusable input set, rebuilt (via `Tty7App::ssh_form_load`) each time
/// a profile is selected. Edits are committed to `Config::ssh_profiles` only on
/// Save, so the form can be abandoned freely. Mirrors the four-core-fields +
/// collapsible jump / forwards / advanced disclosure the old standalone editor
/// exposed.
pub(crate) struct SshProfileForm {
    /// The profile id being edited. A *new* (unsaved) profile carries a freshly
    /// minted id here and is only written to config on Save.
    editing: Uuid,
    /// The group / credential_ref carried over from the profile being edited, so
    /// a Save round-trips fields the form doesn't expose.
    carry_group: Option<String>,
    carry_credential_ref: Option<CredentialRef>,

    // Section expansion (progressive disclosure).
    show_jump: bool,
    show_forwards: bool,
    show_advanced: bool,

    // Core fields.
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    auth: AuthMode,

    // Jump host (a profile name; empty = none).
    jump: Entity<InputState>,

    // Forwards, one rule per line: `L bind_host:bind_port target_host:target_port [desc]`.
    forwards: Entity<InputState>,

    // Advanced text inputs.
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

    // Advanced booleans / tri-states.
    agent_forward: bool,
    x11: bool,
    skip_banner: bool,
    verify_host_keys: Option<bool>,
    warn_on_close: Option<bool>,

    /// Keeps the inputs' change subscriptions alive for this form; dropped (and
    /// re-created) whenever the form is rebuilt for another profile.
    _subs: Vec<Subscription>,
}

/// In-progress capture of a new shortcut for one action (click a Keybindings
/// row). The interceptor lives here so it stays active only while recording;
/// dropping it (capture done / Esc) removes the key swallow.
pub(crate) struct Recording {
    /// The action name whose shortcut is being captured.
    pub(crate) action: String,
    /// The chords captured so far, each a config spec (e.g. `["ctrl-b", "x"]`).
    /// A single chord is the common case; more than one records a sequence like
    /// the tmux preset's `ctrl-b x`. Committed (joined by spaces) after a short
    /// pause with no further keys.
    pub(crate) chords: Vec<String>,
    /// Keeps the keystroke interceptor alive for the duration of the capture.
    pub(crate) _intercept: Subscription,
}

/// Sentinel first row in the bold/italic font pickers meaning "no distinct face
/// — reuse the primary family with synthesized emphasis". Chosen to be an
/// unlikely real font name.
pub(crate) const FONT_DEFAULT_LABEL: &str = "Default (match primary)";

/// Humanize a CamelCase action name for display: "CloseActiveTab" → "Close
/// Active Tab".
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

// ── SSH-profile form parsing helpers (moved here from the standalone editor) ──

/// Parse a `host:port` fragment into a [`HostPort`], or `None` when empty/blank.
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

/// Render a `HostPort` back to `host:port` for the form (empty string for `None`).
fn host_port_text(hp: &Option<HostPort>) -> String {
    hp.as_ref()
        .map(|h| format!("{}:{}", h.host, h.port))
        .unwrap_or_default()
}

/// Split a comma/whitespace list into non-empty items (algorithms, etc.).
fn split_list(s: &str) -> Vec<String> {
    s.split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Split a multiline input into non-empty trimmed lines.
fn split_lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Parse the forwards text area (one rule per line) into [`ForwardRule`]s.
/// Lines that don't parse are skipped rather than failing the whole save.
fn parse_forwards(s: &str) -> Vec<ForwardRule> {
    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(4, char::is_whitespace);
        let kind = match parts.next().map(|k| k.to_ascii_uppercase()) {
            Some(k) if k == "L" || k == "LOCAL" => ForwardKind::Local,
            Some(k) if k == "R" || k == "REMOTE" => ForwardKind::Remote,
            Some(k) if k == "D" || k == "DYNAMIC" => ForwardKind::Dynamic,
            _ => continue,
        };
        let Some(bind) = parts.next().and_then(parse_host_port) else {
            continue;
        };
        // Dynamic ignores the target; Local/Remote need it.
        let target = if kind == ForwardKind::Dynamic {
            HostPort::default()
        } else {
            match parts.next().and_then(parse_host_port) {
                Some(t) => t,
                None => continue,
            }
        };
        let description = parts.next().unwrap_or("").trim().to_string();
        out.push(ForwardRule {
            kind,
            bind,
            target,
            description,
        });
    }
    out
}

/// Render `ForwardRule`s back into the text-area format.
fn forwards_text(rules: &[ForwardRule]) -> String {
    rules
        .iter()
        .map(|r| {
            let kind = match r.kind {
                ForwardKind::Local => "L",
                ForwardKind::Remote => "R",
                ForwardKind::Dynamic => "D",
            };
            let bind = format!("{}:{}", r.bind.host, r.bind.port);
            if r.kind == ForwardKind::Dynamic {
                format!("{kind} {bind} {}", r.description)
                    .trim()
                    .to_string()
            } else {
                let target = format!("{}:{}", r.target.host, r.target.port);
                format!("{kind} {bind} {target} {}", r.description)
                    .trim()
                    .to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build an `InputState` seeded with `value` (single- or multi-line). A free
/// function so `window` auto-reborrows cleanly at each call site.
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
    /// Build the settings tab body: a fixed left sidebar (section nav) beside a
    /// scrollable content area for the selected section. Esc closes the tab.
    pub(crate) fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        // Copy the palette out (Hsla is Copy) so this borrow doesn't outlive into
        // `render_settings_search_results` below, which needs `cx` mutably.
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
            None => return div(), // not a settings tab; nothing to render
        };
        // Live settings-search query (trimmed, lowered). Non-empty swaps the six
        // section links for a cross-section list of matching settings.
        let query = search.read(cx).value().trim().to_lowercase();
        // The theme picker panel only makes sense beside its own page.
        let show_theme_panel = theme_panel_open && section == SettingsSection::Appearance;

        // `TTY7_PROFILE`: time this section's whole element build and, via the
        // aggregated call rate, expose whether the panel is rebuilding once (on a
        // real change) or in a tight `notify` loop. Labelled per section so
        // Appearance's cost stands apart from the lighter pages.
        let prof = crate::ui::perf::enabled()
            .then(|| (std::time::Instant::now(), section.profile_label()));

        // Sidebar nav item that activates a section on click. While a search is
        // active it also carries a trailing `(N)` count of that section's matching
        // settings — the full section nav stays put and is annotated with
        // per-section hit counts, rather than collapsing into a flat result list.
        let nav_item = |label: &'static str, target: SettingsSection, icon: IconName| {
            let view = cx.entity();
            let count = if query.is_empty() {
                0
            } else {
                section_match_count(target, &query)
            };
            let item = SidebarMenuItem::new(label)
                .icon(Icon::new(icon))
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

        // The six section links stay put during search — only their `(N)` suffixes
        // change — so the nav never collapses out from under the user.
        let nav_body = SidebarMenu::new()
            .child(nav_item(
                "Appearance",
                SettingsSection::Appearance,
                IconName::Palette,
            ))
            // Sliders for Terminal (it's the tuning page), the `>_`
            // prompt glyph for Shell (it configures the prompt's
            // program) — the two would otherwise both claim `>_`.
            .child(nav_item(
                "Terminal",
                SettingsSection::Terminal,
                IconName::Settings2,
            ))
            .child(nav_item(
                "Shell",
                SettingsSection::Shell,
                IconName::SquareTerminal,
            ))
            .child(nav_item("SSH", SettingsSection::Ssh, IconName::Globe))
            .child(nav_item(
                "Window & Tabs",
                SettingsSection::WindowTabs,
                IconName::WindowRestore,
            ))
            // The icon set ships no keyboard glyph; CaseSensitive ("Aa")
            // is the closest key-ish cue available.
            .child(nav_item(
                "Keybindings",
                SettingsSection::Keybindings,
                IconName::CaseSensitive,
            ))
            .child(nav_item("About", SettingsSection::About, IconName::Info));

        let sidebar = Sidebar::new("settings-sidebar")
            .collapsible(SidebarCollapsible::None)
            // Match the tab sidebar's default width (`default_sidebar_width`, 220px)
            // so toggling the settings overlay over the vertical rail doesn't shift
            // the left column — narrower than the stock 255px too, which three short
            // items don't need and which reads more native/less hollow.
            .w(px(220.))
            .header(
                v_flex()
                    .w_full()
                    .px_2()
                    .gap_2()
                    // Reserve the title-bar height at the top so the nav rail
                    // reaches the very top of the window (the macOS traffic lights
                    // rest on its surface) with the header clearing them — matching
                    // the tab rail's top zone.
                    .pt(px(crate::ui::app::TITLE_BAR_HEIGHT))
                    .pb_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(header_muted)
                            .child("SETTINGS"),
                    )
                    // Settings search: type a setting or a synonym and each section
                    // below shows how many of its settings match, with the
                    // best-matching section auto-selected (see the search input's
                    // change subscription in `app.rs`). Styled like the tab sidebar's
                    // search — a leading magnifier + a borderless input sitting flush
                    // on the rail surface, no box, so the header reads clean.
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(Icon::new(IconName::Search).small().text_color(header_muted))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Input::new(&search).appearance(false)),
                            ),
                    ),
            )
            .child(nav_body);

        let content = match section {
            SettingsSection::Appearance => self.render_settings_appearance(cx),
            SettingsSection::Terminal => self.render_settings_terminal(cx),
            SettingsSection::Shell => self.render_settings_shell(cx),
            SettingsSection::Ssh => self.render_settings_ssh(cx),
            SettingsSection::WindowTabs => self.render_settings_window_tabs(cx),
            SettingsSection::Keybindings => self.render_settings_keybindings(cx),
            SettingsSection::About => self.render_settings_about(cx),
        };

        // One continuous, flat sheet (no cards) — one document: bold section
        // headers and full-width rules carry the structure, so settings read as a
        // unified document rather than a widget floating in empty space.
        //
        // The SSH section is the exception: it is its own two-column master-detail
        // that fills the pane height, with each column owning its scroll — so it
        // bypasses the shared padded, single-scroll wrapper (which would otherwise
        // give the whole section one outer scrollbar and no definite height for the
        // columns to fill) and is dropped in flush instead.
        let content_pane = if section == SettingsSection::Ssh {
            v_flex()
                .id("settings-content")
                .flex_1()
                .h_full()
                .bg(background)
                .child(content)
        } else {
            v_flex()
                .id("settings-content")
                .flex_1()
                .h_full()
                .bg(background)
                .overflow_y_scroll()
                .child(
                    div()
                        .px_10()
                        .py_8()
                        // Fill the pane edge-to-edge; cap only on very wide windows so
                        // rows never stretch to an unreadable width.
                        .child(div().w_full().max_w(px(860.)).child(content)),
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
            // The Sidebar draws its own right border; no wrapper border here, or
            // the two stack into one thick rule.
            .child(sidebar)
            .child(content_pane)
            // The overlay covers the real title bar, so the window's own drag
            // region is buried. Restore it: a transparent strip across the top
            // band (the height the title bar reserved) that moves the window on
            // drag and zooms it on double-click, exactly like the title bar it
            // stands in for. Driven the same way `TitleBar` does — a press arms a
            // `should_move` flag and the first move calls `start_window_move`
            // (deferring to an actual move keeps a plain click, and double-click,
            // intact); the `WindowControlArea::Drag` tag covers the Windows path.
            .child({
                let should_move = Rc::new(Cell::new(false));
                div()
                    .id("settings-titlebar-drag")
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(crate::ui::app::TITLE_BAR_HEIGHT))
                    .window_control_area(WindowControlArea::Drag)
                    .on_mouse_down(MouseButton::Left, {
                        let should_move = should_move.clone();
                        move |_, _, _| should_move.set(true)
                    })
                    .on_mouse_up(MouseButton::Left, {
                        let should_move = should_move.clone();
                        move |_, _, _| should_move.set(false)
                    })
                    .on_mouse_move(move |_, window, _| {
                        if should_move.replace(false) {
                            window.start_window_move();
                        }
                    })
                    .on_double_click(|_, window, _| window.titlebar_double_click())
            })
            .when(show_theme_panel, |r| r.child(self.render_theme_panel(cx)))
            // Close affordance at the page's top-right corner (Esc and Cmd+, also
            // close) — the intuitive "close this page" spot, and clear of the
            // macOS traffic lights (top-left) and the window controls' zone.
            // Hidden while the theme panel is open: it docks at the same right edge
            // and carries its own ✕, so keeping this one would stack two ✕ there.
            .when(!show_theme_panel, |r| {
                r.child(
                    div().absolute().top(px(6.)).right(px(10.)).occlude().child(
                        Button::new("settings-close")
                            .icon(IconName::Close)
                            .ghost()
                            .small()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_settings(window, cx)),
                            ),
                    ),
                )
            });

        if let Some((start, label)) = prof {
            crate::ui::perf::record(label, start.elapsed());
        }
        root
    }

    /// Just the styled section title (no margin). Shared by `section_header` and
    /// `section_intro` so the two can never drift in size, weight, or color.
    fn header_text(&self, title: &str, cx: &Context<Self>) -> Div {
        div()
            .text_base()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().foreground)
            .child(title.to_string())
    }

    /// A bold section header that introduces a group of settings.
    /// With no cards, the header *is* the unit of grouping — it tells the eye
    /// where one set of related controls begins.
    pub(crate) fn section_header(&self, title: &str, cx: &Context<Self>) -> Div {
        self.header_text(title, cx).mb_4()
    }

    /// A section header paired with its one-line intro as a single unit: the
    /// subtitle sits tight under the title (`gap_1`) and the block leaves a
    /// consistent gap before the first control (`mb_4`). Replaces the ad-hoc
    /// "header, then a loose paragraph" pattern that stranded the subtitle 16px
    /// below its own title (glued instead to the controls) and used a different
    /// bottom margin — `mb_1` here, `mb_2` there — in every section.
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

    /// A full-width hairline between sections, so the page reads as one
    /// continuous sheet rather than stacked boxes.
    pub(crate) fn section_rule(&self, cx: &Context<Self>) -> Div {
        div().h(px(1.)).my_7().bg(cx.theme().border)
    }

    /// One labelled settings row, shared by every section: title + description
    /// in a fixed-width left column, control immediately beside it. A fixed
    /// column (not space-between) keeps label and control visually paired
    /// regardless of window width — space-between on a wide pane stretched the
    /// two apart into a dead gap.
    pub(crate) fn settings_row(
        &self,
        label: impl Into<String>,
        desc: impl Into<String>,
        control: AnyElement,
        cx: &Context<Self>,
    ) -> Div {
        let theme = cx.theme();
        h_flex()
            .items_center()
            .gap_8()
            .py_2()
            .child(
                v_flex()
                    .gap_0p5()
                    .w(px(260.))
                    .flex_shrink_0()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .child(label.into()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(desc.into()),
                    ),
            )
            .child(control)
    }

    /// A segmented control (gpui-component's `ButtonGroup`, outline) for a small
    /// set of mutually-exclusive options — the refined stand-in for a raw row of
    /// radio circles, which read as an unstyled form beside the sheet's tuned
    /// steppers and chips. Joined outline segments with a soft-filled active one
    /// speak the same segmented language as the −│value│+ stepper; `small` pins
    /// every option control to the same 24px height as the selects beside them.
    /// `selected` is the active index; `on_pick` fires with the newly chosen one.
    pub(crate) fn segmented(
        &self,
        id: &'static str,
        options: &'static [&'static str],
        selected: usize,
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Self, usize, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        ButtonGroup::new(id)
            .outline()
            .small()
            .children(options.iter().enumerate().map(|(i, label)| {
                // `(id, i)` keeps each segment's element id unique across the
                // several segmented controls on the page.
                Button::new((id, i)).label(*label).selected(i == selected)
            }))
            .on_click(cx.listener(move |this, clicks: &Vec<usize>, window, cx| {
                // Single-select: `clicks` carries just the newly chosen index.
                if let Some(&ix) = clicks.first() {
                    on_pick(this, ix, window, cx);
                }
            }))
            .into_any_element()
    }

    /// Appearance section: theme, font size, font family.
    fn render_settings_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let foreground = theme.foreground;
        let border = theme.border;
        let hover_bg = theme.secondary.opacity(0.6);
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

        // Unified −/value/+ stepper plus a quiet Reset.
        let step = move |id: &'static str, glyph: &'static str, divider: bool| {
            div()
                .id(id)
                .px_2p5()
                .py_1()
                .text_sm()
                .cursor_pointer()
                .text_color(foreground)
                .when(divider, |s| s.border_l_1().border_color(border))
                .hover(|h| h.bg(hover_bg))
                .child(glyph)
        };
        // One shared height for every small control in this section (matches
        // gpui-component's own Size::Small button height) so the stepper pill
        // and the font-family select sit at the same visual weight instead of
        // each defaulting to its own padding.
        let control_h = px(24.);
        // The −│value│+ pill plus its quiet Reset — one shape shared by the
        // font-size and line-height rows; callers hand in the wired buttons.
        let stepper_row =
            move |dec: Stateful<Div>, value: String, inc: Stateful<Div>, reset: Button| {
                h_flex()
                    .items_center()
                    .justify_start()
                    .w(px(240.))
                    .gap_3()
                    .child(
                        h_flex()
                            .items_center()
                            .h(control_h)
                            .rounded_lg()
                            .bg(stepper_bg)
                            .border_1()
                            .border_color(border)
                            .overflow_hidden()
                            .child(dec)
                            .child(
                                div()
                                    .min_w(px(40.))
                                    // Hairline on the value's left edge so both internal
                                    // seams read (−│value│+); the `+` supplies the right one.
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
                    .child(reset)
                    .into_any_element()
            };
        let font_size_control = stepper_row(
            step("font-dec", "−", false).on_click(
                cx.listener(|this, _, _w, cx| this.change_font_size(-FONT_SIZE_STEP, cx)),
            ),
            format!("{:.0}", font_size),
            step("font-inc", "+", true)
                .on_click(cx.listener(|this, _, _w, cx| this.change_font_size(FONT_SIZE_STEP, cx))),
            Button::new("font-reset")
                .label("Reset")
                .ghost()
                .small()
                .on_click(cx.listener(|this, _, _w, cx| this.reset_font_size(cx))),
        );

        let line_height = self.line_height;
        let line_height_control = stepper_row(
            step("lh-dec", "−", false).on_click(
                cx.listener(|this, _, _w, cx| this.change_line_height(-LINE_HEIGHT_STEP, cx)),
            ),
            format!("{:.2}", line_height),
            step("lh-inc", "+", true).on_click(
                cx.listener(|this, _, _w, cx| this.change_line_height(LINE_HEIGHT_STEP, cx)),
            ),
            Button::new("lh-reset")
                .label("Reset")
                .ghost()
                .small()
                .on_click(cx.listener(|this, _, _w, cx| this.reset_line_height(cx))),
        );

        // One font dropdown, shared shape for primary / bold / italic pickers.
        let font_dropdown = |state: &Entity<SelectState<SearchableVec<String>>>| {
            h_flex()
                .justify_start()
                .w(px(240.))
                .child(
                    Select::new(state)
                        .small()
                        .w(px(180.))
                        .h(control_h)
                        .search_placeholder("Search fonts…")
                        // Cap the popup's own height so browsing doesn't dump the
                        // OS's entire font catalog on screen at once — it just
                        // scrolls from here. Every font is still in the list and
                        // reachable by typing; this only trims what's shown.
                        .menu_max_h(px(224.)),
                )
                .into_any_element()
        };
        let font_family_control = font_dropdown(&font_select);
        let font_bold_control = font_dropdown(&font_bold_select);
        let font_italic_control = font_dropdown(&font_italic_select);
        let ligature_switch = Switch::new("font-ligatures")
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
        // Blink lives here beside the shape — one Cursor home, not "shape is
        // appearance, blink is behavior" split across two pages.
        let blink_switch = Switch::new("cursor-blink")
            .checked(cursor_blink)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_cursor_blink(*on, cx)))
            .into_any_element();

        v_flex()
            .child(self.section_intro(
                "Theme",
                "Pick a color theme. Each one sets its own light or dark look.",
                cx,
            ))
            .child(self.render_current_theme(cx))
            // Custom-theme management (duplicate / edit colors / open folder) is
            // *about* themes, so it lives with the picker rather than stranded at
            // the foot of the page after Cursor.
            .child(self.render_custom_themes(cx))
            .child(self.section_rule(cx))
            .child(self.section_header("Typography", cx))
            .child(self.settings_row(
                "Font Size",
                "Terminal text size in pixels.",
                font_size_control,
                cx,
            ))
            .child(self.settings_row(
                "Line Height",
                "Row spacing as a multiple of the font size.",
                line_height_control,
                cx,
            ))
            .child(self.settings_row(
                "Font Family",
                "Pick from fonts installed on your system.",
                font_family_control,
                cx,
            ))
            .child(self.settings_row(
                "Bold Font",
                "Face for bold text; Default synthesizes it from the primary.",
                font_bold_control,
                cx,
            ))
            .child(self.settings_row(
                "Italic Font",
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
                "Blink cursor",
                "Pulse the cursor while the terminal is focused.",
                blink_switch,
                cx,
            ))
            .into_any_element()
    }

    /// Custom themes section. On an editable theme, the color editor; on a
    /// read-only built-in / import, a "Duplicate to edit" button that forks it
    /// into an editable file. The folder button is always available.
    fn render_custom_themes(&self, cx: &mut Context<Self>) -> AnyElement {
        let editor = self.active_settings().and_then(|s| s.theme_editor.as_ref());

        let folder_button = Button::new("open-themes-folder")
            .label("Open Themes Folder")
            .small()
            .on_click(cx.listener(|this, _, _w, cx| this.open_themes_folder(cx)));

        if let Some(editor) = editor {
            // Snapshot the picker handles so the render borrow of `self` ends.
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
                .child(self.section_header("ANSI colors", cx))
                .children(
                    ansi.into_iter()
                        .map(|(label, state)| self.render_theme_color_row(label, state, cx)),
                )
                .child(h_flex().mt_4().child(folder_button))
                .into_any_element();
        }

        // Read-only theme (built-in or import): offer to duplicate it into an
        // editable copy, plus the folder affordance.
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
                        // Plain (not `.primary()`): a solid near-black fill reads
                        // far too heavy against this soft, mostly-outline sheet —
                        // it matches the "Open Themes Folder" button beside it.
                        Button::new("duplicate-theme")
                            .label("Duplicate to Edit")
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.fork_active_theme(window, cx)
                            })),
                    )
                    .child(folder_button),
            )
            .into_any_element()
    }

    /// One color-editor row: a label paired with its picker. The picker's own
    /// `Change` event (wired in `rebuild_theme_editor`) writes the edit to the
    /// theme file, so the row itself is purely presentational.
    fn render_theme_color_row(
        &self,
        label: String,
        state: Entity<ColorPickerState>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let control = h_flex()
            .items_center()
            .w(px(240.))
            .child(ColorPicker::new(&state).small())
            .into_any_element();
        self.settings_row(label, "", control, cx)
    }

    /// SSH section: saved connection profiles plus the global security toggles
    /// (host-key verification default and warn-on-close; a per-profile override
    /// still wins where set).
    ///
    /// A two-column master-detail (like the theme picker): the **left** column is
    /// a fixed-width, self-scrolling master — Import / Add on top, then the profile
    /// list; the **right** column is the flex-1, self-scrolling detail pane showing
    /// the selected profile's edit form, or — with nothing selected — an empty
    /// state carrying the "pick a profile" hint and the two global security
    /// toggles. Selection is tracked in [`SettingsState::ssh_detail`].
    fn render_settings_ssh(&self, cx: &mut Context<Self>) -> AnyElement {
        let border = cx.theme().border;
        h_flex()
            .size_full()
            .items_start()
            .child(
                // LEFT (master): fixed width, its own scroll, a right divider.
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
                // RIGHT (detail): flex-1, its own scroll.
                v_flex()
                    .id("ssh-detail")
                    .flex_1()
                    .h_full()
                    .overflow_y_scroll()
                    .child(
                        // Clear the title-bar drag strip / close ✕ up top, and cap
                        // the detail width so the form stays readable on wide panes.
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

    /// The left (master) column: the Import / Add buttons on top, then the
    /// saved-profile list (each row selects into the detail pane).
    fn render_ssh_master(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let profiles = cx.global::<Config>().ssh_profiles.clone();
        let detail = self
            .active_settings()
            .map(|s| s.ssh_detail)
            .unwrap_or(SshDetail::None);

        // Import / Add stacked full-width: the long import label doesn't fit beside
        // Add in the narrow column.
        let header = v_flex()
            .gap_2()
            .child(
                // Plain (not `.primary()`): a solid near-black fill reads far too
                // heavy against this soft, mostly-outline sheet — a subtle default
                // fill still reads as the primary create action above outline Import.
                Button::new("ssh-profiles-add")
                    .label("Add profile")
                    .small()
                    .w_full()
                    .on_click(cx.listener(|this, _, window, cx| this.add_new_profile(window, cx))),
            )
            .child(
                Button::new("ssh-profiles-import")
                    .label("Import from ~/.ssh/config")
                    .outline()
                    .small()
                    .w_full()
                    .on_click(cx.listener(|this, _, _w, cx| this.import_ssh_config_profiles(cx))),
            );

        let mut list = v_flex().gap_0p5().w_full();
        if profiles.is_empty() {
            list = list.child(
                div()
                    .py_4()
                    .text_sm()
                    .text_color(muted)
                    .child("No saved profiles yet. Add one, or import from ~/.ssh/config."),
            );
        }
        for p in &profiles {
            let id = p.id;
            let row_idx = id.as_u128() as usize;
            let subtitle = to_connect_string(p);
            let title = if p.name.is_empty() {
                subtitle.clone()
            } else {
                p.name.clone()
            };
            let selected = detail == SshDetail::Profile(id);
            // A group so this row's ⋯ affordance can reveal on hover
            // (progressive disclosure) without touching its neighbours.
            let group_name = SharedString::from(format!("ssh-profile-row-{row_idx}"));
            let hover_group = group_name.clone();
            // Weak handles so the hover ⋯ dropdown and the right-click context
            // menu can drive the same `Tty7App` handlers the inline buttons used.
            let menu_app = cx.entity().downgrade();
            let ctx_app = cx.entity().downgrade();
            list = list.child(
                h_flex()
                    .id(("ssh-profile-row", row_idx))
                    .group(group_name.clone())
                    .items_center()
                    .justify_between()
                    .w_full()
                    .py_2()
                    .px_2()
                    .rounded_md()
                    .when(selected, |r| r.bg(cx.theme().secondary.opacity(0.4)))
                    // A subtle hover fill so the whole row reads as the (clickable)
                    // select affordance; the selected row keeps its own highlight.
                    .when(!selected, |r| {
                        r.hover(|s| s.bg(cx.theme().secondary.opacity(0.2)))
                    })
                    // Left-click anywhere on the row selects it — its edit form
                    // opens in the detail pane. Clicks on the trailing ⋯ are
                    // swallowed by its wrapper, so they don't also start an edit.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
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
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap_0p5()
                            .child(div().text_sm().truncate().child(title))
                            .child(div().text_xs().text_color(muted).truncate().child(subtitle)),
                    )
                    .child(
                        // Trailing ⋯ overflow menu, revealed on row hover. Its
                        // wrapper swallows the mouse-down so opening the menu never
                        // also fires the row's select click.
                        div()
                            .flex_shrink_0()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .when(!selected, move |s| {
                                s.opacity(0.).group_hover(hover_group, |s| s.opacity(1.))
                            })
                            .child(
                                Button::new(("ssh-prof-menu", row_idx))
                                    .icon(IconName::Ellipsis)
                                    .ghost()
                                    .small()
                                    .dropdown_menu_with_anchor(
                                        gpui::Anchor::TopRight,
                                        move |menu, _window, cx| {
                                            Self::ssh_profile_row_menu(
                                                menu,
                                                id,
                                                cx.theme().danger,
                                                &menu_app,
                                            )
                                        },
                                    ),
                            ),
                    )
                    // Right-click anywhere on the row opens the same menu.
                    .context_menu(move |menu, _window, cx| {
                        Self::ssh_profile_row_menu(menu, id, cx.theme().danger, &ctx_app)
                    }),
            );
        }

        v_flex()
            .p_2()
            .gap_2()
            // Clear the title-bar drag strip up top so the buttons stay clickable.
            .pt(px(crate::ui::app::TITLE_BAR_HEIGHT))
            .child(header)
            .child(list)
            .into_any_element()
    }

    /// The right (detail) pane: a selected profile's edit form, or — with nothing
    /// selected — the empty state (a "pick a profile" hint plus the two global
    /// security toggles).
    fn render_ssh_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let detail = self
            .active_settings()
            .map(|s| s.ssh_detail)
            .unwrap_or(SshDetail::None);
        match detail {
            SshDetail::Profile(_)
                if self.active_settings().is_some_and(|s| s.ssh_form.is_some()) =>
            {
                self.render_ssh_profile_form(cx)
            }
            // No selection (or a stale profile whose form is gone): the empty-state
            // hint, then the global security toggles below it.
            _ => v_flex()
                .gap_6()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Select a profile to edit, or add a new one."),
                )
                .child(self.render_ssh_security_block(cx))
                .into_any_element(),
        }
    }

    /// Build the per-profile overflow menu shared by the hover ⋯ dropdown and the
    /// row's right-click context menu: Connect, Copy, Duplicate, then the
    /// destructive Delete — rendered last, set apart by a separator and drawn in
    /// danger red. Each item drives the same `Tty7App` handler the old inline
    /// buttons did, via the weak `app` handle.
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
            .item(PopupMenuItem::new("Copy").on_click({
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

        // Destructive, last, in danger red and set apart by the separator above.
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

    /// Security block: the global host-key verification default and warn-on-close
    /// toggle (both overridable per profile).
    fn render_ssh_security_block(&self, cx: &mut Context<Self>) -> AnyElement {
        let verify = cx.global::<Config>().verify_host_keys;
        let verify_switch = Switch::new("ssh-verify-host-keys")
            .checked(verify)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_verify_host_keys(*on, cx)))
            .into_any_element();

        let warn_on_close = cx.global::<Config>().ssh_warn_on_close;
        let warn_switch = Switch::new("ssh-warn-on-close")
            .checked(warn_on_close)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_ssh_warn_on_close(*on, cx)))
            .into_any_element();

        v_flex()
            .child(self.section_header("Security", cx))
            .child(self.settings_row(
                "Verify host keys",
                "Check each server's key against known_hosts and confirm unknown or \
                 changed keys. A profile can override this. Turning it off disables \
                 host-key checking for the native SSH path.",
                verify_switch,
                cx,
            ))
            .child(self.settings_row(
                "Warn before closing",
                "Ask for confirmation before closing a tab or pane with a live SSH \
                 session. A profile can override this.",
                warn_switch,
                cx,
            ))
            .into_any_element()
    }

    // ── SSH profile edit form (folded into Settings → SSH) ───────────────────

    /// The open SSH edit form, mutably (for section toggles / auth / switches).
    fn ssh_form_mut(&mut self) -> Option<&mut SshProfileForm> {
        self.active_settings_mut().and_then(|s| s.ssh_form.as_mut())
    }

    /// Build the edit-form inputs seeded from `profile` and open the form. A fresh
    /// input set each call (the old set drops with the previous form), so the SSH
    /// section never carries every profile's inputs at once.
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
        let forwards = seed_input(window, cx, &forwards_text(&profile.forwards), true);
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

        // The jump-host summary and the forwards count recompute live from these
        // two inputs, so a keystroke in either re-renders the section.
        let mut subs = Vec::new();
        for input in [&jump, &forwards] {
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
            verify_host_keys: profile.verify_host_keys,
            warn_on_close: profile.warn_on_close,
            _subs: subs,
        };
        let editing = form.editing;
        if let Some(s) = self.active_settings_mut() {
            s.ssh_form = Some(form);
            // Loading a form selects that profile in the master-detail layout, so
            // its row highlights and the detail pane shows the form.
            s.ssh_detail = SshDetail::Profile(editing);
        }
        cx.notify();
    }

    /// Read the edit form back into an [`SshProfile`], preserving the id and the
    /// carried-over group / credential_ref.
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
            forwards: parse_forwards(&form.forwards.read(cx).value()),
            keepalive_interval_s: val(&form.keepalive_interval).parse().ok(),
            keepalive_count_max: val(&form.keepalive_count).parse().ok(),
            connect_timeout_s: val(&form.connect_timeout).parse().ok(),
            warn_on_close: form.warn_on_close,
            skip_banner: form.skip_banner,
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

    /// Save the edit form into `Config::ssh_profiles` (upsert by id).
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

    /// Save the form and return the detail pane to its empty state.
    pub(crate) fn save_ssh_form(&mut self, cx: &mut Context<Self>) {
        self.save_editing_profile(cx);
        if let Some(s) = self.active_settings_mut() {
            s.ssh_form = None;
            s.ssh_detail = SshDetail::None;
        }
        cx.notify();
    }

    /// Discard unsaved edits and return the detail pane to its empty state (Back).
    pub(crate) fn close_ssh_form(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.active_settings_mut() {
            s.ssh_form = None;
            s.ssh_detail = SshDetail::None;
        }
        cx.notify();
    }

    /// Save the current form, then close Settings and connect the saved profile.
    pub(crate) fn save_and_connect_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.save_editing_profile(cx) {
            self.close_settings(window, cx);
            self.connect_ssh_profile(id, window, cx);
        }
    }

    /// Add a fresh blank profile and open it in the edit form.
    pub(crate) fn add_new_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profile = SshProfile::new(String::new());
        self.ssh_form_load(&profile, window, cx);
    }

    /// Duplicate a saved profile (new id, "… (copy)" name) and edit the copy.
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

    /// Delete a saved profile and its frecency entry.
    pub(crate) fn delete_profile(&mut self, id: Uuid, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| {
            cfg.ssh_profiles.retain(|p| p.id != id);
            cfg.ssh_profile_frecency.remove(&id);
        });
        let editing_deleted =
            self.active_settings().map(|s| s.ssh_detail) == Some(SshDetail::Profile(id));
        if let Some(s) = self.active_settings_mut().filter(|_| editing_deleted) {
            // The deleted profile was selected: drop its form and clear the
            // selection back to the empty state.
            s.ssh_form = None;
            s.ssh_detail = SshDetail::None;
        }
        cx.notify();
    }

    /// Import `~/.ssh/config` aliases as profiles (idempotent upsert by name).
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

    /// Copy a saved profile's `user@host:port` to the clipboard (FR-P5).
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

    /// Remove any keychain-stored password for this profile's endpoint
    /// (`user@host:port`). The profile itself is untouched — the next connect will
    /// prompt again. A no-op if nothing was stored. Returns a status line for the
    /// caller to surface as a notification. Credentials are keyed by endpoint, not
    /// profile, so this only matches when the profile pins an explicit user.
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

    /// The inline edit form: four core fields + collapsible jump / forwards /
    /// advanced, rendered below the profile list for the selected profile.
    fn render_ssh_profile_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(form) = self.active_settings().and_then(|s| s.ssh_form.as_ref()) else {
            return div().into_any_element();
        };
        let is_new = !cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .any(|p| p.id == form.editing);
        let title = if is_new {
            "New profile"
        } else {
            "Edit profile"
        };

        let auth_idx = match form.auth {
            AuthMode::Auto => 0,
            AuthMode::Password => 1,
            AuthMode::PublicKey => 2,
            AuthMode::Agent => 3,
            AuthMode::KeyboardInteractive => 4,
        };
        let header = h_flex()
            .items_center()
            .justify_between()
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("ssh-form-back")
                            .label("‹ Back")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, _w, cx| this.close_ssh_form(cx))),
                    )
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("ssh-form-connect")
                            .label("Connect")
                            .outline()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.save_and_connect_profile(window, cx)
                            })),
                    )
                    .child(
                        // Plain, not `.primary()`: keep the soft sheet aesthetic (see
                        // the Add-profile / Duplicate-to-Edit buttons) — a near-black
                        // fill is too jarring here.
                        Button::new("ssh-form-save")
                            .label("Save")
                            .small()
                            .on_click(cx.listener(|this, _, _w, cx| this.save_ssh_form(cx))),
                    ),
            );

        let core = v_flex()
            .gap_3()
            .child(self.settings_row(
                "Name",
                "A label for this connection.",
                Input::new(&form.name).small().into_any_element(),
                cx,
            ))
            .child(
                self.settings_row(
                    "Host",
                    "Hostname or IP address.",
                    h_flex()
                        .gap_2()
                        .child(Input::new(&form.host).small())
                        .child(div().w(px(80.)).child(Input::new(&form.port).small()))
                        .into_any_element(),
                    cx,
                ),
            )
            .child(self.settings_row(
                "User",
                "Login user (blank = resolve at connect).",
                Input::new(&form.user).small().into_any_element(),
                cx,
            ))
            .child(self.settings_row(
                "Auth",
                "Authentication method. Auto tries every applicable method.",
                self.segmented(
                    "ssh-form-auth",
                    &["Auto", "Password", "Key", "Agent", "2FA"],
                    auth_idx,
                    cx,
                    |this, ix, _w, cx| {
                        if let Some(f) = this.ssh_form_mut() {
                            f.auth = match ix {
                                0 => AuthMode::Auto,
                                1 => AuthMode::Password,
                                2 => AuthMode::PublicKey,
                                3 => AuthMode::Agent,
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

    /// A collapsible section header (▸/▾ label + summary), toggling `open`.
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
            section = section.child(self.settings_row(
                "Jump host",
                "Name of another profile to tunnel through (blank = direct).",
                Input::new(&form.jump).small().into_any_element(),
                cx,
            ));
        }
        section.into_any_element()
    }

    fn render_ssh_profile_forwards_section(
        &self,
        form: &SshProfileForm,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let count = parse_forwards(&form.forwards.read(cx).value()).len();
        let mut section = v_flex().child(self.disclosure_header(
            "ssh-sec-fwd",
            "Port forwards",
            &format!("({count})"),
            form.show_forwards,
            cx,
            |this, cx| {
                if let Some(f) = this.ssh_form_mut() {
                    f.show_forwards = !f.show_forwards;
                    cx.notify();
                }
            },
        ));
        if form.show_forwards {
            section = section
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(
                            "One rule per line: L|R|D bind_host:port target_host:port [description]. Dynamic (D) omits the target.",
                        ),
                )
                .child(div().w_full().child(Input::new(&form.forwards).small()));
        }
        section.into_any_element()
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
                Input::new(input).small().into_any_element(),
                cx,
            )
        };

        // Verify host keys / warn-on-close tri-states (Default / On / Off).
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
                    "Forward the local ssh-agent to the session.",
                    Switch::new("ssh-form-agent")
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
                    Switch::new("ssh-form-x11")
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
                    Switch::new("ssh-form-banner")
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
                "Override the global known_hosts check for this profile.",
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
                "Warn on close",
                "Override the global confirm-before-closing for this profile.",
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

    /// Shell section: the program tty7 launches in each new terminal, plus its
    /// launch arguments. Both apply to *newly spawned* panes/tabs — existing
    /// shells keep running until closed. An empty program falls back to the
    /// platform default (the login shell on Unix; PowerShell 7 when installed,
    /// else Windows PowerShell, on Windows).
    fn render_settings_shell(&self, cx: &mut Context<Self>) -> AnyElement {
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

        // Name what an empty Program field falls back to, so the default
        // behaviour is legible without the user having to know it.
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
        // The custom path input only matters for `Custom`; show it there.
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
                "Executable name on PATH or an absolute path. e.g. zsh, fish, pwsh.",
                program_control,
                cx,
            ))
            .child(self.settings_row(
                "Arguments",
                "Space-separated launch flags. e.g. -l for a login shell.",
                args_control,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Working directory", cx))
            .child(self.settings_row(
                "Start in",
                "Inherit the launch dir, your home, or a fixed path.",
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
                    .child("Applies to new tabs and panes; shells already open keep running. A new tab still inherits the active pane's directory."),
            )
            .into_any_element()
    }

    /// Terminal section: how the terminal surface itself behaves — scrolling,
    /// mouse, links, clipboard, notifications. Plain switches and segmented
    /// controls driven straight off the `Config` global (each control's handler
    /// mutates + saves it). Small groups on purpose: each header names exactly
    /// what it contains, so it doubles as the landmark you scan for.
    fn render_settings_terminal(&self, cx: &mut Context<Self>) -> AnyElement {
        let foreground = cx.theme().foreground;
        let cfg = cx.global::<Config>();
        let link_url = cfg.link_url;
        let ssh_loopback_forward = cfg.ssh_loopback_forward;
        let mouse_hide = cfg.mouse_hide_while_typing;
        let focus_follows = cfg.focus_follows_mouse;
        let option_as_alt = cfg.macos_option_as_alt;
        let scroll_mult = cfg.mouse_scroll_multiplier;
        let clip_trim = cfg.clipboard_trim_trailing_spaces;
        let copy_on_select = cfg.copy_on_select;
        let mouse_reporting = cfg.mouse_reporting;
        let bell = cfg.bell;
        // Map the persisted threshold onto its preset radio index (nearest slot
        // for any off-preset value a hand-edit might leave).
        let threshold_idx = match cfg.notify_threshold_secs {
            n if n <= 5 => 0,
            n if n <= 10 => 1,
            n if n <= 30 => 2,
            _ => 3,
        };
        // Map the persisted scrollback depth onto its preset radio index (default
        // to 10k's slot for any off-preset value a hand-edit might leave).
        let scrollback_idx = match cfg.scrollback_limit {
            n if n <= 1_000 => 0,
            n if n <= 10_000 => 1,
            _ => 2,
        };
        let notify_idx = match cfg.notify_on_command_finish {
            NotifyMode::Never => 0,
            NotifyMode::Unfocused => 1,
            NotifyMode::Always => 2,
        };
        let scroll_slider = match self.active_settings() {
            Some(s) => s.scroll_slider.clone(),
            None => return div().into_any_element(),
        };

        let link_switch = Switch::new("term-link-url")
            .checked(link_url)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_link_url(*on, cx)))
            .into_any_element();
        let ssh_loopback_switch = Switch::new("term-ssh-loopback-forward")
            .checked(ssh_loopback_forward)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_ssh_loopback_forward(*on, cx)))
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
        let notify_radio = self.segmented(
            "term-notify",
            &["Never", "When unfocused", "Always"],
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

        let focus_switch = Switch::new("term-focus-follows")
            .checked(focus_follows)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_focus_follows_mouse(*on, cx)))
            .into_any_element();
        let mouse_hide_switch = Switch::new("term-mouse-hide")
            .checked(mouse_hide)
            .on_click(
                cx.listener(|this, on: &bool, _w, cx| this.set_mouse_hide_while_typing(*on, cx)),
            )
            .into_any_element();
        let trim_switch = Switch::new("term-clip-trim")
            .checked(clip_trim)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_clipboard_trim(*on, cx)))
            .into_any_element();
        let copy_on_select_switch = Switch::new("term-copy-on-select")
            .checked(copy_on_select)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_copy_on_select(*on, cx)))
            .into_any_element();
        let mouse_report_switch = Switch::new("term-mouse-report")
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
        let threshold_radio = self.segmented(
            "term-notify-threshold",
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
        // macOS only: the Option/special-character split this toggle resolves
        // doesn't exist on other platforms, where Alt always carries Meta.
        let option_alt_row = cfg!(target_os = "macos").then(|| {
            let switch = Switch::new("term-option-as-alt")
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
        // Slider + a live readout of the current multiplier beside it.
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
                "Let full-screen apps (vim, tmux) handle clicks and scrolling. Off keeps the mouse local; Shift always does for one gesture.",
                mouse_report_switch,
                cx,
            ))
            .when_some(option_alt_row, |v, row| {
                v.child(self.section_rule(cx))
                    .child(self.section_header("Keyboard", cx))
                    .child(row)
            })
            .child(self.section_rule(cx))
            .child(self.section_header("Links", cx))
            .child(self.settings_row(
                "Detect URLs",
                "Underline links on hover and open them on ⌘-click.",
                link_switch,
                cx,
            ))
            .child(self.settings_row(
                "Forward SSH loopback links",
                "When a pane is in SSH, open localhost links through a temporary port forward.",
                ssh_loopback_switch,
                cx,
            ))
            .child(self.section_rule(cx))
            .child(self.section_header("Clipboard", cx))
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
            .child(self.section_rule(cx))
            .child(self.section_header("Bell", cx))
            .child(self.settings_row(
                "Terminal bell",
                "How a bell (^G) is signalled: silenced, a brief flash, or the system sound.",
                bell_control,
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

    /// Window & Tabs section: the app window's lifecycle and tab placement.
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
        let tab_bar_idx = match cfg.tab_bar_position {
            TabBarPosition::Top => 0,
            TabBarPosition::Left => 1,
        };

        let restore_switch = Switch::new("wt-restore-session")
            .checked(restore_session)
            .on_click(cx.listener(|this, on: &bool, _w, cx| this.set_restore_session(*on, cx)))
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

        v_flex()
            .child(self.section_header("Window", cx))
            .child(self.settings_row(
                "Startup window",
                "Window state when tty7 launches.",
                startup_radio,
                cx,
            ))
            .child(self.settings_row(
                "Restore previous session",
                "Reopen the last window's tabs, splits, and directories on launch. Off starts with a single fresh terminal.",
                restore_switch,
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
            .into_any_element()
    }

    /// Theme gallery: one clickable card per theme (built-ins + user files), each
    /// a mini-terminal preview painted in its own colors. The selected card gets a
    /// soft ring + a check; clicking switches the active theme live via
    /// `set_preset`.
    /// The mini terminal preview for a theme: thin "lines of code" bars in the
    /// theme's own colors over its background. Fills its container's width, so a
    /// narrow "Current theme" card and the wider picker panel reuse one shape.
    fn theme_preview(&self, p: &presets::Theme) -> Div {
        let to_u32 = |(r, g, b): (u8, u8, u8)| (r as u32) << 16 | (g as u32) << 8 | b as u32;
        let accent = rgb(p.accent);
        let ansi = |i: usize| rgb(to_u32(p.ansi16[i]));
        let fg = rgb(p.foreground);
        // A "line of code": thin rounded bars whose widths are *fractions* of the
        // preview, so the same shape reads well in the narrow "Current theme" card
        // and the wider picker instead of clustering at the left edge. Rows stay
        // ragged-right like real terminal text.
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

    /// The compact "Current theme" card on the Appearance page: a preview of the
    /// active theme beside its name and light/dark mode, the whole row a click
    /// target that opens the picker panel on the right.
    fn render_current_theme(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let border = theme.border;
        let foreground = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let hover_bg = theme.secondary.opacity(0.5);
        let surface = theme.secondary.opacity(0.28);

        let active_id = cx.global::<Config>().theme_preset.clone();
        let active = presets::by_id(cx, &active_id);
        let name = active.name.clone();
        let mode = if active.dark { "Dark" } else { "Light" };
        let preview = self.theme_preview(&active);
        let open = self.active_settings().is_some_and(|s| s.theme_panel_open);

        div()
            .id("current-theme")
            .mt_1()
            .mb_2()
            .max_w(px(520.))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _w, cx| this.toggle_theme_panel(cx)))
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
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(foreground)
                                    .child(name),
                            )
                            .child(div().text_xs().text_color(muted_fg).child(mode)),
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

    /// The theme picker: a right-hand column of searchable preview
    /// cards. Opened from the "Current theme" card; applying a theme keeps the
    /// panel open (with its own `×`) so several looks can be tried in a row.
    fn render_theme_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let border = theme.border;
        let foreground = theme.foreground;
        let muted_fg = theme.muted_foreground;
        // A hair off the content pane (like the settings rail) so the panel reads
        // as its own surface rather than an extension of the page.
        let bg = theme.sidebar;

        let active_id = cx.global::<Config>().theme_preset.clone();
        let (search, query) = match self.active_settings() {
            Some(s) => (
                s.theme_search.clone(),
                s.theme_search.read(cx).value().trim().to_lowercase(),
            ),
            None => return div().into_any_element(),
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
                Button::new("theme-panel-close")
                    .icon(IconName::Close)
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _, _w, cx| this.close_theme_panel(cx))),
            );

        let subtitle = div()
            .px_4()
            .pb_3()
            .text_xs()
            .text_color(muted_fg)
            .child("Change your current theme.");

        // Plain text input, the same shape the Shell section uses — our own
        // field, not a bespoke pill. The Input fills its parent, but a percent
        // width needs a *definite* one to resolve against, so the wrapper is sized
        // explicitly (panel 300 − px_4 gutters). Placeholder labels it as search;
        // a leading magnifier keeps that reading at a glance.
        let search_box = div().px_4().pb_3().child(
            div().w(px(268.)).child(
                Input::new(&search)
                    .small()
                    .prefix(Icon::new(IconName::Search).small().text_color(muted_fg)),
            ),
        );

        let mut list = v_flex().px_4().pb_4().gap_4();
        for p in presets::all(cx) {
            if !query.is_empty() && !p.name.to_lowercase().contains(&query) {
                continue;
            }
            let id = p.id.clone();
            let is_active = active_id == id;
            let preview = self.theme_preview(&p);
            let click_id = id.clone();
            list = list.child(
                v_flex()
                    .id(SharedString::from(format!("panel-theme-{id}")))
                    .gap_1p5()
                    .cursor_pointer()
                    .child(
                        // Percent width (`w_full` in the preview) only resolves
                        // against a *definite* parent, so pin the card to the
                        // panel's content width (300 − px_4 gutters) — same reason
                        // the search box above is sized explicitly.
                        div()
                            .w(px(268.))
                            .rounded_lg()
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
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_preset(&click_id, window, cx)
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

    /// Keybindings section: the effective shortcut list (defaults + overrides).
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

        // Config-derived state, read into owned values so the `cx` borrow is
        // free for `effective_bindings` and the click listeners below.
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

        // The row currently capturing a shortcut (action + chords so far), and
        // any pending takeover note.
        let recording = self
            .active_settings()
            .and_then(|s| s.recording.as_ref())
            .map(|r| (r.action.clone(), r.chords.clone()));
        let note = self
            .active_settings()
            .and_then(|s| s.rebinding_note.clone());

        // One key glyph as a small keycap, so a shortcut reads like real keys.
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

        // A preset toggle button, highlighted when active.
        let preset_button =
            |id: &'static str, label: &'static str, value: &'static str, on: bool| {
                Button::new(id).label(label).small().selected(on).on_click(
                    cx.listener(move |this, _, _w, cx| this.set_keybinding_preset(value, cx)),
                )
            };
        // A prefix choice button (tmux preset only).
        let prefix_button =
            |id: &'static str, label: &'static str, value: &'static str, on: bool| {
                Button::new(id).label(label).small().selected(on).on_click(
                    cx.listener(move |this, _, _w, cx| this.set_keybinding_prefix(value, cx)),
                )
            };

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
            .child(
                h_flex()
                    .gap_1()
                    .child(preset_button("preset-default", "Default", "default", !tmux))
                    .child(preset_button("preset-tmux", "tmux", "tmux", tmux)),
            );

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
            .child(
                h_flex()
                    .gap_1()
                    .child(prefix_button(
                        "prefix-ctrl-b",
                        "Ctrl-B",
                        "ctrl-b",
                        prefix == "ctrl-b",
                    ))
                    .child(prefix_button(
                        "prefix-ctrl-a",
                        "Ctrl-A",
                        "ctrl-a",
                        prefix == "ctrl-a",
                    )),
            );

        let count = effective.len();
        let mut list = v_flex().mt_2();
        for (i, (action, key)) in effective.into_iter().enumerate() {
            let is_recording = recording.as_ref().is_some_and(|(a, _)| a == &action);
            let is_overridden = overridden.contains(&action);

            // Keycap clusters for a spec: one cluster per whitespace-separated
            // chord (a sequence like `ctrl-b x` draws as two clusters), with a
            // wider gap between clusters than within one.
            let keycaps = |spec: &str| {
                h_flex().gap_2().children(
                    crate::ui::keymap::key_chords(spec)
                        .into_iter()
                        .map(|chord| h_flex().gap_1().children(chord.into_iter().map(&keycap))),
                )
            };

            // Right side: the live capture (chords so far + hint), the keycap
            // sequence, or "—".
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

            // The whole right cell is clickable to start capturing this row.
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
                "Keyboard Shortcuts",
                "Click a shortcut, then press the new keys — it saves after a brief pause. Chain keys for a sequence like Ctrl-B then X. Esc cancels; Backspace removes the last key, or resets to default. Changes apply immediately.",
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

    /// About section: app identity and stack.
    fn render_settings_about(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (foreground, muted_fg) = (theme.foreground, theme.muted_foreground);

        // Startup update check (see `core::update`): a newer release, if one was
        // found, plus the toggle that controls whether we look at all.
        let update = cx
            .try_global::<crate::core::update::UpdateStatus>()
            .and_then(|s| s.available.clone());
        let check_for_updates = cx.global::<Config>().check_for_updates;

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
                            ))),
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
                            .child("The window closes. Your session doesn't."),
                    )
                    .child(div().text_sm().text_color(muted_fg).child(
                        "The window is just a view — your shells run in a persistent daemon, so closing it never kills a session. GPU-rendered through gpui on Zed's alacritty_terminal core.",
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child("GPU-rendered · daemon-backed · shell-aware"),
                    ),
            )
            // Updates: the startup check drops a newer version here if it found
            // one. We never self-update — "Download" just opens the Releases
            // page; the toggle turns the check off (see `core::update`).
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
                        this.child(
                            h_flex()
                                .gap_3()
                                .items_center()
                                .child(div().text_sm().text_color(foreground).child(
                                    format!("Version {} is available.", upd.version),
                                ))
                                .child(
                                    // Match the sibling "Restart Background
                                    // Service…" button (default style, not the
                                    // dark `.primary()` fill) so About reads as
                                    // one panel.
                                    Button::new("download-update")
                                        .label("Download")
                                        .small()
                                        .on_click(cx.listener(|this, _, _w, _cx| {
                                            this.open_releases_page()
                                        })),
                                ),
                        )
                    })
                    .child(div().text_sm().text_color(muted_fg).child(
                        "Check GitHub for a newer release on launch and show a download link here. tty7 never updates itself; the button opens the Releases page.",
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Switch::new("check-updates")
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
            // Manage that daemon. A fresh process is the only way to pick up a
            // macOS permission granted after it started (e.g. Full Disk Access),
            // to recover if it wedges, or to start clean — quitting/reopening the
            // window alone never restarts it. Ends every running session, so the
            // action confirms first.
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
                            .child("Daemon"),
                    )
                    .child(div().text_sm().text_color(muted_fg).child(
                        "Restart the daemon to pick up a newly granted macOS permission, recover if it stops responding, or start from a clean slate. This ends all running sessions; your tabs and layout reopen with fresh shells.",
                    ))
                    .child(
                        h_flex().child(
                            Button::new("restart-daemon")
                                .label("Restart Daemon…")
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
    fn humanize_action_splits_on_capitals() {
        assert_eq!(humanize_action("NewTab"), "New Tab");
        assert_eq!(
            humanize_action("ToggleMaximizePane"),
            "Toggle Maximize Pane"
        );
        assert_eq!(humanize_action("Quit"), "Quit");
    }

    #[test]
    fn forwards_round_trip_through_text() {
        let rules = vec![
            ForwardRule {
                kind: ForwardKind::Local,
                bind: HostPort::new("127.0.0.1", 8080),
                target: HostPort::new("10.0.0.1", 80),
                description: "web".to_string(),
            },
            ForwardRule {
                kind: ForwardKind::Dynamic,
                bind: HostPort::new("127.0.0.1", 1080),
                target: HostPort::default(),
                description: String::new(),
            },
        ];
        let text = forwards_text(&rules);
        let parsed = parse_forwards(&text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].kind, ForwardKind::Local);
        assert_eq!(parsed[0].bind.port, 8080);
        assert_eq!(parsed[0].target.host, "10.0.0.1");
        assert_eq!(parsed[0].description, "web");
        assert_eq!(parsed[1].kind, ForwardKind::Dynamic);
        assert_eq!(parsed[1].bind.port, 1080);
    }

    #[test]
    fn parse_forwards_skips_malformed_lines() {
        // Bad kind, and a Local rule missing its target — both skipped.
        let parsed = parse_forwards("X 1:2 3:4\nL 127.0.0.1:9000\nR 0.0.0.0:80 10.0.0.2:8080");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, ForwardKind::Remote);
    }

    #[test]
    fn parse_host_port_handles_blank_and_ports() {
        assert!(parse_host_port("  ").is_none());
        let hp = parse_host_port("example.com:2222").unwrap();
        assert_eq!(hp.host, "example.com");
        assert_eq!(hp.port, 2222);
        // No colon → host only, port 0.
        assert_eq!(parse_host_port("host").unwrap().port, 0);
    }
}

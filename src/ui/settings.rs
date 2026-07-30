//! The Settings tab UI (Cmd+,): a sidebar of sections beside a scrollable
//! content pane. This module owns the panel's *state types* and its *rendering*
//! only; the lifecycle (opening/closing the tab, committing the font family,
//! applying theme/font changes) lives in `app.rs`, where it can touch the
//! shell's tabs and panes. The render methods extend `Tty7App` from here so the
//! window shell stays focused on tab/pane orchestration.

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

/// Which section of the settings panel is currently selected in the sidebar.
/// Sections are named for the *object* being configured (the appearance, the
/// terminal, the window) — never for a property class like "Behavior", which
/// reads fine but predicts nothing about what's inside.
///
/// Two of these were rearranged because the old split didn't survive contact
/// with a user asking "which page is that on?":
///
/// * **Shell** used to be its own page holding three settings, and nothing
///   distinguished "the Terminal page" from "the Shell page" from the outside.
///   Its rows are now Terminal's first group — the program a pane launches is a
///   property of the terminal, not a peer of it. (It also freed the word
///   "Shell", which the menu bar was simultaneously using for its File menu.)
/// * **Input** is new. Completion, history search, the Option/Meta split and
///   selection/clipboard behaviour were scattered through the bottom of the
///   Terminal page under four headers; they're the app's most distinctive
///   surface and they now have a name you can look for.
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
    /// Every section, in nav order. The single source of truth for "what
    /// sections exist" — [`best_matching_section`] used to carry its own
    /// hand-written copy of this list and had silently fallen two behind.
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

    /// A `&'static` label for `TTY7_PROFILE` aggregation, so each section's build
    /// cost and rebuild rate report under their own line.
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
        // ── Appearance ──────────────────────────────────────────────────────
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
        // ── Terminal ────────────────────────────────────────────────────────
        SearchEntry {
            section: Terminal,
            title: "Program",
            keywords: "shell binary zsh bash fish pwsh powershell executable launch",
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
        // ── Input ───────────────────────────────────────────────────────────
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
        // ── SSH ─────────────────────────────────────────────────────────────
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
        // ── Agents ──────────────────────────────────────────────────────────
        // Titles mirror `HookAgent::display_name()`, which is what each row is
        // rendered with; the mechanism word (hooks/plugin/extension) lives in
        // `keywords`. Pinned by `agent_rows_are_in_the_search_index`.
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
        // ── Window & Tabs ───────────────────────────────────────────────────
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
            // Both spellings of the chord: the prompt this turns off is reached
            // by ⌘W on macOS and Ctrl-W everywhere else, and the user types
            // whichever one their own keyboard just used.
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
        // ── Keybindings / About ─────────────────────────────────────────────
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
            title: "How sessions work",
            keywords: "session daemon detach persist background close quit stop delete workspace layout survive reboot tmux",
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
///
/// Driven by [`SettingsSection::ALL`] rather than a hand-written list: the old
/// literal here omitted SSH and Agents, so searching "claude" or "known hosts"
/// annotated the nav with a match count and then refused to go there.
pub(crate) fn best_matching_section(query: &str) -> Option<SettingsSection> {
    SettingsSection::ALL
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
    /// Background-image opacity slider; present only while the theme has an
    /// image (wired to `Tty7App::set_theme_image_opacity`).
    pub(crate) image_opacity_slider: Option<Entity<SliderState>>,
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
    /// Command template run when ⌘/Ctrl-clicking a file link (Links section). Empty
    /// clears the override, restoring the built-in "open in default app".
    pub(crate) link_file_command_input: Entity<InputState>,
    /// Mouse-scroll multiplier slider (Terminal section).
    pub(crate) scroll_slider: Entity<SliderState>,
    /// Global window-opacity slider (Appearance's Window section). Shows the
    /// effective value; dragging sets the config override.
    pub(crate) window_opacity_slider: Entity<SliderState>,
    /// The color editor for the effective (on-screen) theme, or `None` when
    /// that theme is read-only (a built-in / import).
    pub(crate) theme_editor: Option<ThemeEditor>,
    /// Whether the theme picker panel is open beside the content pane
    /// (Appearance section only). Toggled from the theme card(s).
    pub(crate) theme_panel_open: bool,
    /// Which theme choice the open picker panel writes to (see [`ThemeSlot`]).
    /// Set by the card that opened the panel.
    pub(crate) theme_panel_slot: ThemeSlot,
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
    /// Live filter for the SSH master list. Non-empty narrows the list to hosts
    /// whose name or address matches, and force-expands every group — results
    /// hiding inside a collapsed group is the same as not finding them.
    pub(crate) ssh_filter: Entity<InputState>,
    /// Group keys (see [`ssh_group_key`]) whose section in the master list is
    /// collapsed. Empty = everything expanded.
    pub(crate) ssh_collapsed_groups: std::collections::HashSet<String>,
    /// The `user@host[:port]` box in the SSH section's empty state. An empty
    /// pane whose only content is "select something" wastes the widest column on
    /// the page; connecting is what someone opening this section came to do.
    pub(crate) ssh_quick_connect: Entity<InputState>,
    /// Which machine the Agents section is showing and acting on.
    /// [`HostId::LOCAL`] until the user picks one of the connected remotes.
    pub(crate) agent_hooks_host: HostId,
    /// Install state of each agent's hook integration on
    /// [`Self::agent_hooks_host`]. Cached — captured when the panel opens,
    /// re-read when the section or the machine is selected, and updated after
    /// each install/uninstall — because reading it is a file read per agent,
    /// and on a remote machine that is a round trip per agent.
    pub(crate) agent_hooks_states: AgentHooksView,
    /// Discriminates the load whose answer is allowed to land. Switching
    /// machines while a read is in flight would otherwise let the old
    /// machine's rows arrive under the new machine's name.
    pub(crate) agent_hooks_seq: u64,
    /// Outcome of the last Agents-section hook action (install summary or
    /// error), shown under that agent's row. Replaced by the next action.
    pub(crate) agent_hooks_note: Option<(crate::core::agent_hooks::HookAgent, String)>,
    pub(crate) _subs: Vec<Subscription>,
}

/// What Settings → Agents has to show for the machine it is pointed at.
///
/// Three states rather than a `Vec` that is empty when it doesn't know: reading
/// a remote machine's install state is a round trip per agent, so "still asking"
/// and "asked, nothing installed" are genuinely different answers and rendering
/// them the same is how a page silently lies for a second.
#[derive(Clone)]
pub(crate) enum AgentHooksView {
    /// The read is in flight.
    Loading,
    /// One row per hook-capable agent, in
    /// [`crate::core::agent_hooks::HookAgent::ALL`] order.
    Ready(Vec<AgentHookRow>),
    /// The machine can't be acted on, and the sentence says which hop gave up
    /// (a failure is a resting state, not a blank).
    Unavailable(String),
}

/// One agent's row, as read off a particular machine.
#[derive(Clone)]
pub(crate) struct AgentHookRow {
    pub(crate) agent: crate::core::agent_hooks::HookAgent,
    pub(crate) state: crate::core::agent_hooks::HooksState,
    /// The file the integration lives in *on that machine*, `~`-abbreviated.
    /// Resolved in the background with the rest of the read — it depends on the
    /// machine's own home directory and separator, which render cannot ask for.
    pub(crate) target: String,
}

/// One entry in the Agents section's machine picker.
#[derive(Clone)]
pub(crate) struct AgentHooksMachine {
    pub(crate) host: HostId,
    pub(crate) label: String,
}

/// The theme choice a picker card / the picker panel targets. `Manual` is the
/// single `Config::theme_preset` (sync-with-system off); `Light` / `Dark` are
/// the two follow-system slots (`Config::theme_preset_light` / `_dark`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemeSlot {
    Manual,
    Light,
    Dark,
}

/// The SSH section's right-pane selection (see [`SettingsState::ssh_detail`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshDetail {
    /// Nothing selected — the right pane shows the quick-connect empty state.
    None,
    /// The settings every host inherits. Its own row at the top of the master
    /// list rather than a block pinned under the form: "each host starts from
    /// these and can override one" is a thing the list's shape can say, and a
    /// paragraph under an unrelated form cannot.
    Defaults,
    /// A profile's edit form (paired with `ssh_form`, keyed by the profile id).
    Profile(Uuid),
}

/// The master list's bucket for `profile.group`: imported aliases first, then
/// any user-defined group, then the ungrouped ones. The key is the raw `group`
/// value (`""` for ungrouped) so it can key the collapsed-set directly.
fn ssh_group_key(p: &SshProfile) -> &str {
    p.group.as_deref().unwrap_or("")
}

/// The header text for a group key. The import bucket is labelled by the file
/// it mirrors — `Imported from ssh_config` describes a past action, and this
/// group is a live link to a file the user edits elsewhere.
fn ssh_group_label(key: &str) -> &str {
    match key {
        crate::core::ssh_config::IMPORTED_GROUP => "~/.ssh/config",
        "" => "In tty7",
        other => other,
    }
}

/// Sort rank for a group key: imported first, ungrouped last, custom groups in
/// between (alphabetical among themselves).
fn ssh_group_rank(key: &str) -> u8 {
    match key {
        crate::core::ssh_config::IMPORTED_GROUP => 0,
        "" => 2,
        _ => 1,
    }
}

/// Whether a profile survives the master list's filter. `query` is already
/// trimmed and lowercased. Matches the name and the address separately rather
/// than the rendered `user@host:port` line, so typing a port still finds the
/// host but typing `@` doesn't match everything.
fn ssh_row_matches(p: &SshProfile, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let hit = |s: &str| s.to_lowercase().contains(query);
    hit(&p.name) || hit(&p.host) || hit(&p.user) || hit(&p.port.to_string())
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

    // Port forwards, one row of inputs per rule (see [`ForwardRuleForm`]).
    forwards: Vec<ForwardRuleForm>,

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
    shell_integration: bool,
    verify_host_keys: Option<bool>,
    warn_on_close: Option<bool>,

    /// Keeps the inputs' change subscriptions alive for this form; dropped (and
    /// re-created) whenever the form is rebuilt for another profile.
    _subs: Vec<Subscription>,
}

/// One port-forward rule as live inputs.
///
/// The whole set used to be a single multi-line text box in which each rule had
/// to be typed as `L bind_host:port target_host:port [description]` — a syntax
/// nothing on the page taught, and the only field in this form that could
/// silently drop what you entered (an unparsable line was skipped on save).
pub(crate) struct ForwardRuleForm {
    pub(crate) kind: ForwardKind,
    pub(crate) bind_host: Entity<InputState>,
    pub(crate) bind_port: Entity<InputState>,
    pub(crate) target_host: Entity<InputState>,
    pub(crate) target_port: Entity<InputState>,
    pub(crate) description: Entity<InputState>,
}

impl ForwardRuleForm {
    /// Read the row back into a rule, or `None` when it is too incomplete to
    /// connect: a listener needs a port, and everything but Dynamic needs a
    /// target. The UI flags such a row rather than dropping it quietly.
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

    /// Whether the row has anything typed in it at all. An untouched row added
    /// by "Add rule" is not an error — it just hasn't been filled in yet.
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

/// How the link-click modifier is spelled in the Links copy. It's gpui's
/// `secondary` (see `Modifiers::secondary`), so it must read ⌘ on macOS and
/// Ctrl on Windows/Linux — same split `key_tokens` uses for keycaps.
#[cfg(target_os = "macos")]
const LINK_MODIFIER_LABEL: &str = "⌘";
#[cfg(not(target_os = "macos"))]
const LINK_MODIFIER_LABEL: &str = "Ctrl";

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

/// The five inputs of a forward row, in tab order. Used to subscribe the row
/// (and nothing else needs to know the field names).
fn forward_row_inputs(row: &ForwardRuleForm) -> [&Entity<InputState>; 5] {
    [
        &row.bind_host,
        &row.bind_port,
        &row.target_host,
        &row.target_port,
        &row.description,
    ]
}

/// Build one forward row's inputs, seeded from `rule`. Port `0` seeds an empty
/// box rather than a literal `0` — the stored default for "unset".
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

/// [`seed_input`] with a placeholder — the forward rows carry no labels of
/// their own, so the hint text is what says which box is which.
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
    pub(crate) fn render_settings(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
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

        // The section links stay put during search — only their `(N)` suffixes
        // change — so the nav never collapses out from under the user.
        let nav_body = SidebarMenu::new()
            .child(nav_item(
                "Appearance",
                SettingsSection::Appearance,
                Icon::new(IconName::Palette),
            ))
            // The `>_` prompt glyph for Terminal, which now owns the shell
            // program; the "Aa" glyph is the closest thing the icon set has to
            // a keyboard for Input.
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
            // The icon set ships no keyboard glyph; CaseSensitive ("Aa")
            // is the closest key-ish cue available.
            .child(nav_item(
                "Keybindings",
                SettingsSection::Keybindings,
                Icon::new(IconName::CaseSensitive),
            ))
            // Not `IconName::Info`: `icons/info.svg` is overridden app-wide with
            // the detail panel's "panel with two lines" glyph, which reads as a
            // document, not as *About*. This row keeps the circled `i`.
            .child(nav_item(
                "About",
                SettingsSection::About,
                Icon::empty().path("icons/circle-info.svg"),
            ));

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
                            // Laid out to land on the nav rows below it rather than
                            // on the header's own inset: a `SidebarMenuItem` is
                            // `p_2` + a 16px icon + `gap_x_2`, so its label starts
                            // 32px into the rail. Matching that takes all three of
                            // these — the magnifier at the rows' 16px (not `small`,
                            // which is 14 and left the glyph reading a size below
                            // the column it heads), the same 8px gap after it, and
                            // `pl_0` on the input, which otherwise adds `input_px`
                            // (12px at the default size) whether or not it draws a
                            // box. Without them the placeholder sat 6px right of
                            // every label under it.
                            .gap_2()
                            // Stock magnifier, not tty7's: this page's glyphs run at
                            // 16px, where the detail panel's redraw reads thin and
                            // its handle stubby. See `assets::STOCK_PREFIX`.
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

        // One continuous, flat sheet (no cards) — one document: bold section
        // headers and full-width rules carry the structure, so settings read as a
        // unified document rather than a widget floating in empty space.
        //
        // The SSH section is the exception: it is its own two-column master-detail
        // that fills the pane height, with each column owning its scroll — so it
        // bypasses the shared padded, single-scroll wrapper (which would otherwise
        // give the whole section one outer scrollbar and no definite height for the
        // columns to fill) and is dropped in flush instead.
        // A `flex_1` pane still defaults to `min-width: auto`, so on a narrow
        // window it refuses to shrink below its content's intrinsic width and
        // shoves the fixed 300px theme panel (and its close `×`) off the right
        // edge. `min_w_0` lets the pane yield so the panel stays fully on-screen.
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
                        // Cap the column tight enough (640px) that a row's
                        // right-aligned control stays visually paired with its
                        // label — the cap is what makes `settings_row`'s
                        // space-between layout safe.
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
            // The Sidebar draws its own right border; no wrapper border here, or
            // the two stack into one thick rule.
            .child(sidebar)
            .child(content_pane)
            // The overlay covers the real title bar, so the window's own drag
            // region is buried. Restore it: a transparent strip across the top
            // band (the height the title bar reserved) that moves the window on
            // drag and zooms it on double-click, exactly like the title bar it
            // stands in for. `window_move_gesture` owns the gesture and the
            // reasoning behind it; the #221 failure was worst here, because this
            // strip *is* the whole top band with no immune caption beside it.
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
            // Close affordance at the page's top-right corner (Esc and Cmd+, also
            // close) — the intuitive "close this page" spot, and clear of the
            // macOS traffic lights (top-left) and the window controls' zone.
            // Hidden while the theme panel is open: it docks at the same right edge
            // and carries its own ✕, so keeping this one would stack two ✕ there.
            .when(!show_theme_panel, |r| {
                r.child(
                    // A full chrome tile, because it stands in the same corner as
                    // the title bar's own: a `small` icon button is 24px, which
                    // reads undersized next to the 34px window-control tiles this
                    // spot belongs to. `right` is the window-control zone's own
                    // margin rather than the content inset — what this has to
                    // clear here is the controls, not a text column. `top`
                    // centres it in the title bar's band.
                    div()
                        .absolute()
                        .top(px((TITLE_BAR_HEIGHT - TILE_SIZE) / 2.))
                        .right(px(10.))
                        .occlude()
                        .child(
                            Button::new("settings-close")
                                .icon(Icon::new(IconName::Close))
                                .ghost()
                                // Sizing the button, not the icon: `Button::render`
                                // overwrites whatever size the icon was handed.
                                // See `BUTTON_ICON_SCALE`.
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
    /// on the left, control right-aligned. Space-between is safe here only
    /// because both hosting columns are capped — the main content column at
    /// 640px and the SSH detail pane at 720px — so the two never stretch apart
    /// into a dead gap the way they did on an uncapped pane; widen either cap
    /// and every row inside it stretches with it. A soft full-row
    /// hover fill makes each row read as one scannable unit — the same quiet
    /// highlight the sidebar and menus use; negative side margins let that fill
    /// bleed past the text edge while labels stay aligned with the section
    /// headers above.
    pub(crate) fn settings_row(
        &self,
        label: impl Into<String>,
        desc: impl Into<String>,
        control: AnyElement,
        cx: &Context<Self>,
    ) -> Div {
        let theme = cx.theme();
        let desc = desc.into();
        h_flex()
            .items_center()
            .justify_between()
            .gap_8()
            .py_2()
            .px_2p5()
            .mx_neg_2p5()
            .rounded_lg()
            .hover(|h| h.bg(gpui::rgb(cx.global::<presets::Surfaces>().window.hover)))
            .child(
                v_flex()
                    .gap_0p5()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .child(label.into()),
                    )
                    // Rows without a description (the theme color editor) stay
                    // single-line instead of carrying an empty text child.
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

    /// A segmented control for a small set of mutually-exclusive options — the
    /// refined stand-in for a raw row of radio circles, which read as an unstyled
    /// form beside the sheet's tuned steppers and chips. Joined segments in a
    /// single outlined track, one of them filled, speak the same segmented
    /// language as the −│value│+ stepper right beside them; the 24px height
    /// matches the selects in the same rows. `selected` is the active index;
    /// `on_pick` fires with the newly chosen one.
    ///
    /// # Why this is hand-rolled
    ///
    /// It used to be gpui-component's `ButtonGroup::outline()` with
    /// `Button::selected`, and that is what issue #197 was reported against. That
    /// path derives the selected segment's fill from `Theme::input` and gives it
    /// the *same* border and the *same* label color as its unselected siblings —
    /// so the entire selection signal was one fill, and that fill came from a
    /// grey unrelated to the active theme. On Dracula it measured **1.03:1**.
    ///
    /// `Theme::input` is now themed (see `ui::theme::apply_theme`), which fixes
    /// the stock control for inputs and selects. But a segmented control is the
    /// one place in the app where several options sit visibly side by side with a
    /// *static* selection, so it is precisely where a fill alone is not enough
    /// (see `presets::Surface`) — and the stock button exposes no way to vary the
    /// label's weight. Owning the 30 lines is cheaper than a fork patch, and it
    /// puts the control on the same ladder every hand-rolled surface reads.
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

    /// [`Self::segmented`] for a control that does *not* sit on the settings
    /// sheet. The track paints its own opaque ground, so it has to be told which
    /// one: dropped on the right panel's sunk rail, a window-surface track reads
    /// as a faintly darker box cut out of the column it sits in — and every rung
    /// above it was derived against the wrong ground.
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
        // Taken by name rather than as a `&'static str` so per-row controls (the
        // forward rules) can carry an id derived from their index.
        let id: SharedString = id.into();
        let on_pick = std::rc::Rc::new(on_pick);
        let count = options.len();
        h_flex()
            .id(gpui::ElementId::Name(id.clone()))
            .h(px(24.))
            .rounded(rounding::TRACK_RADIUS)
            .border_1()
            .border_color(border)
            // The track paints its own ground rather than letting the sheet show
            // through. Every rung of the ladder was derived against this colour,
            // so painting it is what makes those ratios true — a control that
            // leaves its ground to whatever it happens to be composited over is
            // the shape of the bug this whole change is about.
            .bg(gpui::rgb(sf.base))
            // A backstop for content overflow, and nothing more. This used to be
            // what shaped the end segments' fills to the track's rounding, and it
            // cannot do that: gpui's overflow mask is a square, unantialiased
            // scissor (issue #236, see `ui::rounding`). The segments carry their
            // own radii below.
            .overflow_hidden()
            .children(options.iter().enumerate().map(|(i, label)| {
                let active = i == selected;
                let on_pick = on_pick.clone();
                // The two end segments cap the track, so their fills have to draw
                // the corner themselves — one border-width tighter than the
                // track's own radius, so the arc nests inside the border instead
                // of bulging past it into the square clip.
                let corners =
                    rounding::segment_corners(i, count, rounding::TRACK_RADIUS, rounding::HAIRLINE);
                h_flex()
                    // A per-segment id keeps each one unique across the several
                    // segmented controls on the page.
                    .id(gpui::ElementId::NamedInteger(id.clone(), i as u64))
                    .items_center()
                    .justify_center()
                    .h_full()
                    .px_2p5()
                    .text_sm()
                    .cursor_pointer()
                    .rounded_corners(corners)
                    // Hairlines *between* segments only — the track already owns
                    // its outer edge, and a border on the first segment would
                    // double it.
                    .when(i > 0, |s| s.border_l_1().border_color(border))
                    // Both channels, every time. The fill locates the selection in
                    // the row; the label color and weight say it is the one — and
                    // keep saying it on a translucent window, where the fill is
                    // washing over whatever is behind the sheet.
                    .when(active, |s| {
                        s.bg(gpui::rgb(sf.selected))
                            .text_color(gpui::rgb(sf.text_selected))
                            .font_weight(FontWeight::MEDIUM)
                    })
                    .when(!active, |s| {
                        s.text_color(gpui::rgb(sf.text_resting))
                            .hover(|h| h.bg(gpui::rgb(sf.hover)))
                    })
                    // Pressed reads past selected, so pushing the segment that is
                    // already chosen still acknowledges the click.
                    .active(|s| s.bg(gpui::rgb(sf.pressed)))
                    .child(*label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        on_pick(this, i, window, cx);
                    }))
            }))
            .into_any_element()
    }

    /// Appearance section: theme, font size, font family.
    fn render_settings_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let foreground = theme.foreground;
        let border = theme.border;
        // Hover comes off the ladder; `stepper_bg` stays a soft resting tint —
        // it decorates a container rather than signalling a state, which is the
        // one job an alpha-multiplied grey is still fine for.
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

        // Unified −/value/+ stepper plus a quiet Reset. `slot` is the glyph's
        // place in the three-slot track (−│value│+): it draws the internal
        // hairline, and — because a hover fill in an end slot would otherwise
        // square off the track's corner (issue #236) — the corner radii.
        //
        // `h_full` rather than `py_1` is load-bearing for that second job. A
        // padded, auto-height glyph box measures 31px (14px text × gpui's φ line
        // height, plus 8px of padding) against a 22px content box, so the track's
        // `items_center` centres it and `overflow_hidden` crops the overhang —
        // the fill still reaches the corner, but the box's own rounded corner is
        // 4½px outside the visible strip, where it does nothing. Pinning the box
        // to the track's content height puts the arc back where the corner is.
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
        // One shared height for every small control in this section (matches
        // gpui-component's own Size::Small button height) so the stepper pill
        // and the font-family select sit at the same visual weight instead of
        // each defaulting to its own padding.
        let control_h = px(24.);
        // The −│value│+ pill plus its quiet Reset — one shape shared by the
        // font-size and line-height rows; callers hand in the wired buttons.
        // Reset sits *before* the pill: with controls right-aligned, the pill
        // holds the row's hard right edge and the quiet action tucks inboard.
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
                            // Overflow backstop only — `step` rounds its own
                            // corners, because this clip is square (see
                            // `ui::rounding`).
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

        // One font dropdown, shared shape for primary / bold / italic pickers.
        let font_dropdown = |state: &Entity<SelectState<SearchableVec<String>>>| {
            Select::new(state)
                .small()
                .w(px(180.))
                .h(control_h)
                .search_placeholder("Search fonts…")
                // Cap the popup's own height so browsing doesn't dump the
                // OS's entire font catalog on screen at once — it just
                // scrolls from here. Every font is still in the list and
                // reachable by typing; this only trims what's shown.
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
        // Blink lives here beside the shape — one Cursor home, not "shape is
        // appearance, blink is behavior" split across two pages.
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
            // Custom-theme management (duplicate / edit colors / open folder) is
            // *about* themes, so it lives with the picker rather than stranded at
            // the foot of the page after Cursor.
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

    /// Window section (Appearance): the global opacity slider and blur switch
    /// that apply to every theme, then the inactive-pane dimming switch. The
    /// first two are config *overrides* — until touched they follow the active
    /// theme's own `opacity`/`blur`, and "Follow theme" clears them back to that
    /// state; the dimming switch is a plain flag no theme carries a value for,
    /// so it sits below that button and "Follow theme" leaves it alone.
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
            // Not "Window": Settings → Window & Tabs owns that word for the
            // window's lifecycle, and two groups called Window on two pages is
            // how a user ends up on the wrong one.
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
            // Only offered while an override is active; otherwise the values
            // already follow the theme and the button would be a no-op.
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
            // Below "Follow theme", which resets the two rows above it and not
            // this one — a plain setting with no theme value behind it.
            .child(self.settings_row(
                "Dim inactive panes",
                "Fade unfocused panes in a split so the active one stands out.",
                dim_switch,
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
            .label("Open themes folder")
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
            let image_opacity_slider = editor.image_opacity_slider.clone();

            // The theme's current image, for the filename label and the
            // opacity readout (the slider owns its own thumb position).
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
                        // it matches the "Open themes folder" button beside it.
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

    /// One color-editor row: a label paired with its picker. The picker's own
    /// `Change` event (wired in `rebuild_theme_editor`) writes the edit to the
    /// theme file, so the row itself is purely presentational.
    fn render_theme_color_row(
        &self,
        label: String,
        state: Entity<ColorPickerState>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let control = ColorPicker::new(&state).small().into_any_element();
        self.settings_row(label, "", control, cx)
    }

    /// SSH section: saved connection profiles plus the global security toggles
    /// (host-key verification default and warn-on-close; a per-profile override
    /// still wins where set).
    ///
    /// A two-column master-detail (like the theme picker): the **left** column is
    /// a fixed-width, self-scrolling master — Import / Add on top, then the profile
    /// list; the **right** column is the flex-1, self-scrolling detail pane showing
    /// the selected profile's edit form (or a "pick a profile" hint) with the
    /// global security defaults always below. Selection is tracked in
    /// [`SettingsState::ssh_detail`].
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

    /// The left (master) column: a Hosts header carrying the add / overflow
    /// affordances, a live filter, then the list — `Defaults` pinned on top and
    /// every saved host bucketed by group into a collapsible section.
    ///
    /// The filter leads, and Add / Import shrank to icon affordances, because
    /// that is the order the column is actually used in: past a dozen hosts,
    /// finding one *is* the job. Two full-width buttons on top read as a page
    /// header while pushing the content that matters below the fold.
    fn render_ssh_master(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        // Rows in this list paint on the settings sheet, i.e. the window surface.
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
        // Which profiles have a connected pane right now: each row's dot, and the
        // count a collapsed group header keeps showing.
        let live = self.live_ssh_profiles(cx);
        let menu_app = cx.entity().downgrade();

        let header = v_flex()
            .gap_2()
            .child(
                // The same weight every other section leads with. Tried as the nav
                // rail's small-caps label instead, and it read as a sub-header of
                // the nav rather than the title of a column: every other page in
                // Settings opens with a title at this size, and this column is
                // where this page starts. So it gets the line to itself.
                self.header_text("Hosts", cx),
            )
            .child(
                // One toolbar row: the filter, then the two affordances that act on
                // the list. The borderless magnifier-then-input is the nav header's
                // settings search, laid out the same way — a boxed field would be
                // the only outlined control on a sheet that has none, and would read
                // as a different kind of search from the one two columns to its left.
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        // Stock magnifier, not tty7's: at this size the redraw
                        // reads thin and its handle stubby. See `assets::STOCK_PREFIX`.
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
                                    // Stock `⋯`, not tty7's: the redraw's filled
                                    // `r=2` dots smear at this size. See the row
                                    // menu below and `assets::STOCK_PREFIX`.
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

        // Bucket by group, keeping each group's config order. Filtering happens
        // before bucketing, so a group the query empties drops out whole instead
        // of leaving a header standing over nothing.
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

        // Defaults sits above the groups and outside the filter: it owns the
        // security toggles, and hiding those behind a query nobody thinks to type
        // is how a setting becomes undiscoverable.
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
            // A live query force-expands every group: a match hiding inside a
            // collapsed section is the same as no match at all.
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
            // Clear the title-bar drag strip up top so the buttons stay clickable.
            .pt(px(crate::ui::app::TITLE_BAR_HEIGHT))
            .child(header)
            .child(list)
            .into_any_element()
    }

    /// The master column's overflow menu: the `~/.ssh/config` link lives here
    /// rather than on a permanent full-width button, because it is a once-in-a-
    /// while action and the list beneath it is not.
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

    /// One collapsible group header in the master list. Collapsed, it keeps
    /// showing how many of its hosts are connected — folding a section away
    /// should hide the rows, not the fact that something in there is live.
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

    /// One host row: status dot, name over `user@host:port`, and a hover `⋯`.
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

    /// The shared shape of a master-list row (Defaults and every host). `dot`
    /// is `None` for rows that can't be connected; `menu_for` adds the hover `⋯`
    /// and right-click menu for a saved profile.
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
            // The window ladder, both channels — see issue #197: multiplying a
            // soft grey by alpha is how a fill silently disappears.
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
                        // A hollow ring when idle, so the dot column reads as a
                        // status slot rather than appearing only for live hosts
                        // and shunting every other row's text left.
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
                            // The label channel: a selected row steps up in colour
                            // and weight, so which row is loaded reads from the
                            // type and not from the fill alone.
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

        // `.context_menu()` wraps the row in a different element type, so the two
        // cases can't be a `when_some` — they're branched into `AnyElement` here.
        let Some(id) = menu_for else {
            return row.into_any_element();
        };
        // Weak handles so the hover `⋯` dropdown and the right-click menu drive
        // the same handlers.
        let menu_app = cx.entity().downgrade();
        let ctx_app = cx.entity().downgrade();
        let row_idx = id.as_u128() as usize;
        row.child(
            // The wrapper swallows the mouse-down so opening the menu never also
            // fires the row's select click.
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

    /// Saved profiles with a connected pane open right now, by id. Read off the
    /// live panes rather than tracked separately, so it can't go stale.
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

    /// Point the detail pane at the global defaults, dropping any open edit form.
    pub(crate) fn select_ssh_defaults(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.active_settings_mut() {
            s.ssh_form = None;
            s.ssh_detail = SshDetail::Defaults;
        }
        cx.notify();
    }

    /// Collapse / expand one group of the master list.
    ///
    /// Collapsing the group that holds the current selection hands the detail
    /// pane back to Defaults: otherwise the form stays open on a host whose row
    /// is no longer anywhere on screen, and nothing on the page says which host
    /// is being edited.
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

    /// The right (detail) pane: a selected profile's edit form, or — with nothing
    /// selected — a "pick a profile" hint. The global security defaults render
    /// below either state: tucked into the empty state alone they vanished the
    /// moment a profile was selected, so they were easy to never discover.
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
            // No selection (or a stale profile whose form is gone).
            _ => self.render_ssh_empty_state(cx),
        }
    }

    /// The detail pane with nothing selected: a quick-connect box, and — when
    /// `~/.ssh/config` holds aliases tty7 hasn't linked — an offer to link them.
    ///
    /// This replaces a one-line "select a profile to edit" hint that left the
    /// widest column on the page doing nothing.
    fn render_ssh_empty_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let Some(input) = self.active_settings().map(|s| s.ssh_quick_connect.clone()) else {
            return div().into_any_element();
        };
        let target = input.read(cx).value().trim().to_string();
        let parsed = crate::core::ssh_profile::parse_quick_connect(&target);
        let saved = cx.global::<Config>().ssh_profiles.len();

        // How many `~/.ssh/config` aliases aren't in the list yet. Read on render:
        // the file is small, this section is not on a hot path, and a stale count
        // would advertise work that is already done.
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
                            // Off until the box holds something that parses: a
                            // Connect that can only fail is worse than one that
                            // says it isn't ready.
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

        // No extra top padding: the detail pane already clears the title bar, and
        // the heading here has to land on the same baseline as `Hosts` beside it.
        body.into_any_element()
    }

    /// Connect the empty state's quick-connect target, closing Settings first so
    /// the new session is what's on screen.
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

    /// The `Defaults` row's detail: what every host inherits, plus the state of
    /// the `~/.ssh/config` link. Its own page rather than a block pinned under
    /// the profile form, where it read as part of whichever host was open.
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

    /// Build the per-profile overflow menu shared by the hover ⋯ dropdown and the
    /// row's right-click context menu: Connect, Copy address, Duplicate, then the
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
    /// toggle (both overridable per profile). Always visible in the detail pane,
    /// under the form or the empty-state hint.
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

        // Every input in the form is subscribed, not just the few whose values
        // are echoed elsewhere: the header's Save button is enabled by comparing
        // the whole form against the saved profile, so any field going stale
        // would leave Save claiming there is nothing to write.
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

    /// Save the form, leaving it open on the same host.
    ///
    /// It used to close back to an empty pane. With the host list permanently
    /// beside the form that reads as the selection being thrown away; staying
    /// put also lets the now-disabled Save double as the "saved" acknowledgement.
    pub(crate) fn save_ssh_form(&mut self, cx: &mut Context<Self>) {
        self.save_editing_profile(cx);
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
        let editing = form.editing;
        let muted = cx.theme().muted_foreground;
        let success = cx.theme().success;

        // The header identifies the host and offers the one action this page
        // exists for. It used to say "Edit profile" beside a ‹ Back — a title
        // that named the *screen*, on a screen whose subject is a machine.
        let saved = cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .find(|p| p.id == editing)
            .cloned();
        let collected = self.ssh_form_collect(cx);
        // A never-saved profile is always dirty; otherwise compare field by field
        // so Save reads as "there is something to save".
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
                            // Off with nothing to write: the button is the page's
                            // unsaved-changes indicator, so it has to be honest.
                            .disabled(!dirty)
                            .on_click(cx.listener(|this, _, _w, cx| this.save_ssh_form(cx))),
                    )
                    .child(
                        // The one action this page exists for, so it carries the
                        // solid fill — unlike the master column's Add, which sits
                        // over a list and would shout.
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
                    // Explicit widths on every text control: `settings_row` right-aligns
                    // the control in a shrink-to-fit slot, so a bare Input has no
                    // definite width to fill. 260px matches the Shell section's inputs.
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
                    // Host + port split the shared 260px control width.
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
            1 => "1 rule, opened with the session".to_string(),
            n => format!("{n} rules, opened with the session"),
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
                // The direction letters carry the whole meaning of a rule, and
                // `L`/`R` are the one pair people reliably mix up.
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

    /// One forward rule: direction, listener, target, description, remove.
    fn render_forward_rule_row(
        &self,
        idx: usize,
        row: &ForwardRuleForm,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        // Dynamic listens locally and proxies wherever the client asks, so it has
        // no fixed target. The boxes stay in place (dimmed) rather than
        // disappearing, so switching direction doesn't reflow the row.
        let needs_target = row.kind != ForwardKind::Dynamic;
        let kind_idx = match row.kind {
            ForwardKind::Local => 0,
            ForwardKind::Remote => 1,
            ForwardKind::Dynamic => 2,
        };
        // Filled in but not connectable — flagged here rather than dropped
        // silently on save, which is what the old text box did.
        let incomplete = row.collect(cx).is_none() && !row.is_blank(cx);

        // `xsmall` rather than the sheet's usual `small`: five controls share this
        // row, and 24px is the height the segmented track beside them is fixed at.
        // The row reads as one compact table cell — that internal alignment beats
        // matching the full-width single inputs in the rows above.
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

    /// Append a blank forward rule to the open form and subscribe its inputs.
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

    /// Drop one forward rule from the open form.
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
                // Same explicit control width as the core fields above — a bare
                // Input has nothing to fill in the row's right-aligned slot.
                div()
                    .w(px(260.))
                    .child(Input::new(input).small())
                    .into_any_element(),
                cx,
            )
        };

        // Verify host keys / warn-on-close tri-states (Default / On / Off).
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
                    "Forward the local ssh-agent to the session.",
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
                // Name the value `Default` actually resolves to. "Overrides the
                // global setting" tells you a mechanism exists but not what it
                // currently does, which is the only part worth reading here.
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

    /// The Shell group at the top of the Terminal section: the program tty7
    /// launches in each new pane, its launch arguments, and where a fresh shell
    /// starts. All apply to *newly spawned* panes/tabs — existing shells keep
    /// running until closed. An empty program falls back to the platform default
    /// (the login shell on Unix; PowerShell 7 when installed, else Windows
    /// PowerShell, on Windows).
    ///
    /// This used to be a section of its own, which left a three-row page and no
    /// way for a user to guess whether a given knob was filed under "Terminal"
    /// or under "Shell". The program a pane runs is a property of the terminal,
    /// so it opens the Terminal page instead.
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

    /// Terminal section: what a pane runs and how the terminal surface itself
    /// behaves — the shell, scrolling, the mouse, the bell, links. Plain
    /// switches and segmented controls driven straight off the `Config` global
    /// (each control's handler mutates + saves it). Small groups on purpose:
    /// each header names exactly what it contains, so it doubles as the landmark
    /// you scan for.
    ///
    /// Typing, selection and the clipboard used to live down here too, under
    /// four more headers; they moved to their own Input section, which is both
    /// findable by name and short enough to read in one screen.
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
        // Map the persisted scrollback depth onto its preset radio index (default
        // to 10k's slot for any off-preset value a hand-edit might leave).
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

    /// Input section: everything about putting text *in* and taking text *out* —
    /// the completion and history menus at the prompt, the Option/Meta split,
    /// and how selection reaches the clipboard.
    ///
    /// A section of its own because these are the settings that distinguish tty7
    /// from a plain terminal, and they were previously the last four groups of a
    /// seven-group Terminal page — findable only by scrolling past everything
    /// else, and not findable by search at all (completion and history search
    /// had no index entries).
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
        // macOS only: the Option/special-character split this toggle resolves
        // doesn't exist on other platforms, where Alt always carries Meta.
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

    /// Agents section: a machine picker, then one row per hook-capable agent on
    /// that machine — install state + actions per row, copy kept terse.
    ///
    /// The picker is first because everything under it is *about* the chosen
    /// machine: the paths, the states, and what Install writes. An agent running
    /// in a remote workspace's pane runs on the remote box and reads that box's
    /// `~/.claude/settings.json`, so installing here and expecting status there
    /// was the whole gap this page closes.
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
            // A spinner would be four agents' worth of motion for a read that is
            // usually instant; the page just says what it is doing and keeps its
            // shape, so nothing jumps when the rows arrive.
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
            // A resting state that says which hop gave up and what to do
            // next, rather than rows that would silently write nowhere.
            AgentHooksView::Unavailable(reason) => {
                return page
                    .child(div().py_4().text_sm().text_color(warning).child(reason))
                    .into_any_element();
            }
            AgentHooksView::Ready(rows) => {
                for (i, row) in rows.into_iter().enumerate() {
                    let agent = row.agent;
                    // Status: a colored dot + one word; the dot is the only color
                    // on the page, so state reads at a glance.
                    let (dot_color, status_text) = match row.state {
                        HooksState::NotInstalled => (muted_fg, "Not installed"),
                        HooksState::Installed => (success, "Installed"),
                        HooksState::Outdated => (warning, "Outdated"),
                    };
                    // The primary action reads as what it will *do* from this
                    // state.
                    let primary_label = match row.state {
                        HooksState::NotInstalled => "Install",
                        HooksState::Installed => "Reinstall",
                        HooksState::Outdated => "Update",
                    };
                    let row_note = note
                        .as_ref()
                        .filter(|(for_agent, _)| *for_agent == agent)
                        .map(|(_, text)| text.clone());

                    // items_end: the whole stack shares the row's right edge, so
                    // status, buttons, and note line up across every agent row.
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
                        // Width-capped so a long note (error text) wraps instead
                        // of inflating the shrink-proof control column and
                        // crushing the label to zero width.
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

    /// The Agents section's machine picker: this computer plus every connected
    /// remote, one chosen at a time.
    ///
    /// `None` when this computer is the only machine there is — a picker with a
    /// single choice is a control that asks a question with one answer, and the
    /// page below it already says where the files go.
    ///
    /// Hand-rolled rather than [`Self::segmented`] because the options are
    /// machines, not a fixed `&'static [&'static str]` — but it reads off the
    /// same interaction ladder, so it is the same control the rest of the sheet
    /// speaks.
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
                                // Both channels, every time: the fill locates the
                                // selection, the label colour and weight say it is
                                // the one — and keep saying it on a translucent
                                // window, where the fill washes over the desktop.
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
                // A saved machine that isn't connected is absent from the row above,
                // and an absence explains nothing. Say the count and the next move
                // rather than listing fifty `~/.ssh/config` aliases, most of which
                // are git transports that could never host a workspace anyway.
                .when(offline > 0, |col| {
                    col.child(div().text_xs().text_color(muted_fg).child(format!(
                        "{offline} more saved machine{} not connected — open a workspace on one to \
                     install its hooks there.",
                        if offline == 1 { " is" } else { "s are" }
                    )))
                }),
        )
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
        // Notifications are app-level, not terminal-level: the tray menu already
        // exposed the same `NotifyMode` at the top of its own menu while the
        // setting itself sat at the bottom of the Terminal page.
        let notify_idx = match cfg.notify_on_command_finish {
            NotifyMode::Never => 0,
            NotifyMode::Unfocused => 1,
            NotifyMode::Always => 2,
        };
        // Map the persisted threshold onto its preset radio index (nearest slot
        // for any off-preset value a hand-edit might leave).
        let threshold_idx = match cfg.notify_threshold_secs {
            n if n <= 5 => 0,
            n if n <= 10 => 1,
            n if n <= 30 => 2,
            _ => 3,
        };
        let notify_radio = self.segmented(
            "wt-notify",
            // Same order and casing as the tray's Notifications submenu, which
            // writes this very setting — the two used to disagree on both.
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
            // "Session" already means "a shell running in the background" all
            // over this app; using it here for "the saved arrangement of tabs"
            // made the one word mean two things on the same page. The thing
            // being restored is the layout.
            .child(self.settings_row(
                "Restore last layout",
                "Reopen the last window's tabs, splits, and directories on launch. Off starts with a single fresh terminal.",
                restore_switch,
                cx,
            ))
            // Phrased around what stays true either way: the prompt is there to
            // teach that closing isn't ending, so the row that turns it off is
            // the last chance to say so.
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
            // Phrased around what *stays*: the worry this row answers is "will
            // turning it off cost me the branch and the numbers", and the
            // answer is no — only the click goes.
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

    /// The theme choice block on the Appearance page: the "Sync with system"
    /// switch, then either the single manual-theme card or — while following
    /// the OS — one card per light/dark slot.
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

    /// One compact theme card: a preview of the slot's theme beside its caption
    /// (kind + light/dark mode for the manual card, the slot's role for the
    /// follow-system cards), its name, and its six chromatic ANSI swatches; the
    /// whole row a click target that opens the picker panel on the right,
    /// aimed at this slot.
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
        // A user file (duplicated or dropped in the themes folder) vs a built-in.
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
            // The slot cards are captioned by their role; the one matching the
            // current OS appearance is the theme actually on screen.
            ThemeSlot::Light if !crate::ui::theme::system_dark(cx) => {
                format!("Light mode · {kind} · Active")
            }
            ThemeSlot::Light => format!("Light mode · {kind}"),
            ThemeSlot::Dark if crate::ui::theme::system_dark(cx) => {
                format!("Dark mode · {kind} · Active")
            }
            ThemeSlot::Dark => format!("Dark mode · {kind}"),
        };
        // The six chromatic ANSI slots (red…cyan) as tiny swatches — the part of
        // a theme the mini preview's few bars can't show, and what actually
        // distinguishes two same-background themes at a glance.
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

        let (search, query, slot) = match self.active_settings() {
            Some(s) => (
                s.theme_search.clone(),
                s.theme_search.read(cx).value().trim().to_lowercase(),
                s.theme_panel_slot,
            ),
            None => return div().into_any_element(),
        };
        let config = cx.global::<Config>();
        // Guard against a slot that no longer exists in the current mode (the
        // sync switch flipped while the panel was open re-aims it, but stale
        // state must still render something sensible).
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
                // The panel is docked to the window's top edge, so this `×` sits
                // inside the settings overlay's stand-in title-bar strip — an
                // absolute `WindowControlArea::Drag` band across the top 40px
                // (see `root` below). On Windows that band is `HTCAPTION`, and
                // unless something on top registers a mouse-blocking hitbox the
                // OS takes the press as a window-drag and the button's `on_click`
                // never fires. `occlude()` stops hit-testing here, the same way
                // the tab-strip chips and the page's own `×` do. No-op elsewhere;
                // the rest of the header still drags the window.
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

        // Plain text input, the same shape the Shell section uses — our own
        // field, not a bespoke pill. The Input fills its parent, but a percent
        // width needs a *definite* one to resolve against, so the wrapper is sized
        // explicitly (panel 300 − px_4 gutters). Placeholder labels it as search;
        // a leading magnifier keeps that reading at a glance.
        let search_box = div().px_4().pb_3().child(
            div().w(px(268.)).child(
                Input::new(&search)
                    .small()
                    // Stock magnifier — same reason as the page header's.
                    .prefix(
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
            // Here the preview sits *flush* inside the card's border (the
            // "Current theme" card pads it, so it keeps its own 8px there). Flush
            // means its corner has to nest one hairline inside the card's, or it
            // bulges past the border into the square overflow clip — the corner
            // then reads as a hard step instead of an arc (issue #236).
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
                        // Percent width (`w_full` in the preview) only resolves
                        // against a *definite* parent, so pin the card to the
                        // panel's content width (300 − px_4 gutters) — same reason
                        // the search box above is sized explicitly.
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

        // Preset and prefix are each a one-of-two choice among visible siblings —
        // a segmented control, and now built as one. They used to be loose
        // `Button::selected` pairs, which put them on gpui-component's
        // `tokens.button_active`: another field nothing set, so the "on" button
        // wore a stock grey with no relation to the theme (issue #197's failure
        // mode, in a different corner of the same page).
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

    /// "How sessions work": the four-line explanation of the app's own model —
    /// what closing a window does, what Stop does, what Delete does, what Quit
    /// does.
    ///
    /// This is tty7's central idea and the thing that most surprises a user
    /// arriving from another terminal, and until now it was explained *only*
    /// inside the confirmation dialogs — that is, at the moment the user is
    /// already committing to an action, and never before. Stating it once, in
    /// the one page that describes what the app is, means the dialogs confirm a
    /// model the user has already met instead of teaching it under pressure.
    ///
    /// Deliberately a plain definition list rather than settings rows: nothing
    /// here is configurable, and giving it switch-shaped chrome would suggest
    /// otherwise.
    fn render_session_model(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (foreground, muted_fg) = (theme.foreground, theme.muted_foreground);

        let entry = |term: &'static str, meaning: &'static str| {
            v_flex()
                .gap_0p5()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(foreground)
                        .child(term),
                )
                .child(div().text_xs().text_color(muted_fg).child(meaning))
        };

        v_flex()
            .mt_6()
            .gap_2()
            .child(self.section_rule(cx))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(foreground)
                    .child("How sessions work"),
            )
            .child(div().text_xs().text_color(muted_fg).child(
                "Your shells run in a background daemon, not in this window. That is what lets them outlive a quit or a reboot — and it means \"close\" and \"end\" are different things here.",
            ))
            .child(
                v_flex()
                    .mt_2()
                    .gap_3()
                    .child(entry(
                        "Closing a window (⌘W on the last tab)",
                        "Detaches the workspace. Every shell keeps running; the workspace waits on the home page and in the title-bar menu.",
                    ))
                    .child(entry(
                        "Quitting tty7 (⌘Q)",
                        "Same deal, for every window. Nothing running is interrupted.",
                    ))
                    .child(entry(
                        "Stop Workspace",
                        "Ends that workspace's shells but keeps its layout, so you can start it again with fresh ones.",
                    ))
                    .child(entry(
                        "Delete Workspace",
                        "Ends the shells and forgets the layout. The only step here you can't undo.",
                    )),
            )
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
                    // Mirrors the README's positioning line and stack sub-line, so
                    // the app and the repo describe tty7 in the same words.
                    .child(
                        div()
                            .text_sm()
                            .text_color(foreground)
                            .child("A terminal workbench: shells, sessions, SSH, coding agents."),
                    )
                    .child(div().text_sm().text_color(muted_fg).child(
                        "Editor-grade input in every shell, sessions that survive quits and reboots without tmux, a native SSH stack with profiles and port forwarding, and live status for panes running coding agents.",
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child("Pure Rust · GPU rendering on Zed's gpui · VT core from Alacritty"),
                    ),
            )
            .child(self.render_session_model(cx))
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
                                    // Match the sibling "Restart daemon…" button
                                    // (default style, not the dark `.primary()`
                                    // fill) so About reads as one panel.
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
                        "Check GitHub for a newer release on launch and show it here. tty7 never updates itself — downloading happens on the Releases page.",
                    ))
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
                        "Restart the daemon on this computer to pick up a newly granted macOS permission, recover if it stops responding, or start from a clean slate. This ends all running sessions here; your tabs and layout reopen with fresh shells. A remote machine's server is restarted from its own menu in the workspace switcher.",
                    ))
                    .child(
                        h_flex().child(
                            Button::new("restart-daemon")
                                .label("Restart daemon…")
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

    /// Every section must carry at least one index entry, or the search box can
    /// annotate the nav with a count it can never jump to — and, worse, a whole
    /// page of settings becomes unreachable by search.
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

    /// `best_matching_section` must be able to reach every section — it used to
    /// be driven by a hand-written list that had fallen behind by two.
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

    /// Settings that had no index entry at all before this pass — searching for
    /// any of them returned an empty result on a page that plainly had the knob.
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
            ("open files with", Terminal),
            ("bell", Terminal),
            ("known_hosts", Ssh),
            ("claude", Agents),
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

    /// The close-confirmation toggle is the one people go looking for *after*
    /// the dialog has annoyed them, so it has to be reachable by what they'd
    /// type in that moment — not just by its own title.
    #[test]
    fn close_confirmation_toggle_is_findable() {
        // Not a bare "confirm": SSH's own close warning owns that word just as
        // legitimately, and the nav's per-section counts are what disambiguate.
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

    /// The index names rows, so a title that no longer matches the rendered row
    /// sends the user to the right page and then leaves them hunting. This
    /// pins the ones that had drifted (the index said "Working directory"; the
    /// row says "Start in").
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

    /// The Agents rows are titled by [`HookAgent::display_name`], so the index
    /// is derived rather than pinned: every hook-capable agent must have an
    /// Agents-section entry under exactly that name. Adding an agent to
    /// `HookAgent::ALL` without indexing it — how Grok Build became
    /// unsearchable — fails here, as does renaming an agent without moving its
    /// index entry.
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

    /// A host is findable by every part of the address it is displayed with,
    /// not just its name — an imported alias often *is* its hostname.
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

    /// The filter is case-insensitive on a lowercased query, which is what the
    /// master column hands it.
    #[test]
    fn the_host_filter_ignores_case() {
        let mut p = SshProfile::new("Prod-Web");
        p.host = "Example.COM".to_string();
        assert!(ssh_row_matches(&p, "prod"));
        assert!(ssh_row_matches(&p, "example.com"));
    }

    /// Imported aliases lead, ungrouped hosts trail, and anything the user named
    /// sits between them — so the `~/.ssh/config` bucket is never buried.
    #[test]
    fn group_buckets_sort_imported_first_and_ungrouped_last() {
        let mut keys = vec!["", "Work", crate::core::ssh_config::IMPORTED_GROUP];
        keys.sort_by_key(|k| ssh_group_rank(k));
        assert_eq!(
            keys,
            vec![crate::core::ssh_config::IMPORTED_GROUP, "Work", ""]
        );
    }

    /// The import bucket is labelled by the file it mirrors: it is a live link
    /// to something edited elsewhere, not a record of a past import.
    #[test]
    fn group_labels_name_the_file_and_the_app() {
        assert_eq!(
            ssh_group_label(crate::core::ssh_config::IMPORTED_GROUP),
            "~/.ssh/config"
        );
        assert_eq!(ssh_group_label(""), "In tty7");
        assert_eq!(ssh_group_label("Work"), "Work");
    }

    /// A profile's bucket comes off its own `group`, with `None` collapsing to
    /// the same key the ungrouped section uses.
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
        // No colon → host only, port 0.
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
        // Wrapped in a `Root` like `main.rs` does — the gpui-component widgets on
        // the settings sheet reach for it on the window.
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

    /// Appearance is where issue #236's controls live: the cursor-shape segmented
    /// track, the −/value/+ steppers and the theme picker's flush previews all
    /// compute their own corner radii now (`ui::rounding`) instead of leaning on
    /// `overflow_hidden`, and the stepper's glyph boxes were re-laid-out to
    /// `h_full` so those radii land on the track's content box.
    ///
    /// gpui's test platform lays out and paints but never rasterizes, so this
    /// cannot assert what the corners *look* like. What it can do is put the page
    /// through a real layout and paint pass — twice, at two widths, so the tracks
    /// are measured more than once — which is the cheapest seam that catches a
    /// panic or a broken constraint in that arithmetic before a human ever sees
    /// the window.
    #[gpui::test]
    fn appearance_section_lays_out_with_its_rounded_controls(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);
        app.update_in(&mut vcx, |app, window, cx| {
            app.open_settings_section(SettingsSection::Appearance, window, cx);
        });

        vcx.simulate_resize(size(px(1100.), px(800.)));
        vcx.run_until_parked();

        // The flush-mounted theme previews only render with the picker docked
        // open, so the page has to be paint-tested in both states to reach every
        // site this change touched.
        app.update_in(&mut vcx, |app, _, cx| {
            if let Some(s) = app.active_settings_mut() {
                s.theme_panel_open = true;
            }
            cx.notify();
        });
        // Narrow enough that the theme list re-measures against a different
        // available width than the pass above.
        vcx.simulate_resize(size(px(720.), px(560.)));
        vcx.run_until_parked();

        let section = vcx.update(|_, cx| app.read(cx).active_settings().map(|s| s.section));
        assert!(
            matches!(section, Some(SettingsSection::Appearance)),
            "the panel should still be on Appearance after two paint passes",
        );
    }
}

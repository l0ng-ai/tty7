//! The window shell: a transparent unified title bar carrying the tab strip,
//! with the active terminal filling the rest. Owns all tabs (each its own PTY).

use gpui::{
    App, Axis, Bounds, Context, Entity, Focusable, Pixels, PromptLevel, Subscription, Window, div,
    img, prelude::*, px,
};
use gpui_component::color_picker::{ColorPickerEvent, ColorPickerState};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::select::{SearchableVec, SelectEvent, SelectState};
use gpui_component::slider::{SliderEvent, SliderState};
use gpui_component::{
    ActiveTheme as _, IndexPath, InteractiveElementExt as _, TitleBar, WindowExt as _,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use crate::core::actions::*;
use crate::core::config::{
    Config, CursorStyle as ConfigCursorStyle, NewTabPosition, RightPanelTab, ShellConfig,
    TabBarPosition,
};
use crate::core::session::{
    Session, SessionAxis, SessionPane, SessionTab, WorkspaceId, WorkspaceStore,
};
use crate::core::shells::DetectedShell;
use crate::core::ssh_config;
use crate::core::window_state::{WindowGeometry as _, WindowState};
use crate::daemon::protocol::{RemoteContext, ShellSpec, ssh_option_takes_value};
use crate::terminal::view::{ChildExited, TerminalView};
use crate::ui::palette::{
    ChromeState, Command, CommandGroup, CommandKind, PaletteEvent, PaletteView,
};
use crate::ui::pane::{CloseOutcome, Dir, Pane, PaneSlot};
use crate::ui::presets::Fill;
use crate::ui::settings::{
    Recording, SettingsSection, SettingsState, ThemeEditor, humanize_action,
};
use crate::ui::theme::{apply_theme, set_menus, window_background};

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

/// The chrome tile rhythm: a square hit box centred on a glyph, in two sizes —
/// the chrome's controls (title bar, rail, panel tabs, code header), and the
/// smaller ones that sit *inside* a panel's body, which have to read as
/// subordinate to the header above them.
///
/// The glyph sizes are nominal — the viewBox, not the mark. What they were
/// picked to land is ~10.8pt of actual *ink*, measured off a screenshot against
/// the macOS traffic lights (12pt across) in the same frame. That is a hair under
/// VS Code / Windsurf, which measure 12pt the same way, and well over the 8.4pt
/// these tiles drew before [`crate::ui::tab_strip::BUTTON_ICON_SCALE`] — the size
/// they had been pinned to regardless of what any call site asked for.
pub(crate) const TILE_SIZE: f32 = 32.;
pub(crate) const TILE_GLYPH: f32 = 13.;
pub(crate) const TILE_SIZE_SM: f32 = 24.;
pub(crate) const TILE_GLYPH_SM: f32 = 11.;

/// Line-art glyphs need a bigger nominal size than framed ones to draw the same
/// ink: in lucide's 24-unit box `plus` spans 5→19 where `panel-left` spans 3→21,
/// so at one shared size the "+" reads a fifth smaller than the tile beside it.
/// Sized off the measured ratios (58% against 72%), not the viewBox arithmetic.
/// (No `_SM` counterpart: every body-scale tile currently carries a framed mark.)
pub(crate) const TILE_GLYPH_LINE: f32 = 16.;

/// Distance from a tile's edge to the glyph inside it — what anything lining a
/// tile up with text or with the window edge subtracts from the inset it wants,
/// so the *glyph* lands on the line rather than the invisible hit box around it.
///
/// Deliberately the nominal gap, not the distance to the glyph's ink. Counting
/// the transparent margin lucide leaves inside the mark is more accurate about
/// where the ink is, and useless: it makes `TILE_PAD` bigger than
/// [`CONTENT_INSET`], which drove [`tile_trailing_inset`] to under a pixel and
/// left the hover capsule looking sheared off against the window edge. The
/// capsule is a thing you can see; it can't be pushed off screen to put the
/// glyph a truer 2px to the right.
pub(crate) const TILE_PAD: f32 = (TILE_SIZE - TILE_GLYPH) / 2.;
pub(crate) const TILE_PAD_SM: f32 = (TILE_SIZE_SM - TILE_GLYPH_SM) / 2.;

/// Help-menu destinations. The README already points people at these; the app
/// itself offered none of them, so the only in-product way to reach the docs or
/// the chat was to already know the URL.
const DOCS_URL: &str = "https://github.com/l0ng-ai/tty7#readme";
const DISCORD_URL: &str = "https://discord.gg/s3dethqz2V";
const ISSUES_URL: &str = "https://github.com/l0ng-ai/tty7/issues/new";

/// The one content inset the whole window aligns to: the rail's text and icons,
/// the title bar's chrome glyphs, and the side panels all start (or end) here, so
/// every vertical edge in the chrome falls on one of two lines rather than the
/// five slightly different ones each surface used to pick for itself.
pub(crate) const CONTENT_INSET: f32 = 12.;

/// Smallest gap between a tile's hit box — the capsule its hover and selected
/// states paint — and the window edge it sits against.
///
/// A floor, because the two rules that set that gap disagree once a tile is big
/// relative to [`CONTENT_INSET`]: aligning the glyph wants the box pushed out by
/// [`TILE_PAD`], and at `TILE_SIZE` 32 against an inset of 12 that leaves the
/// capsule flush with the edge, reading as clipped rather than aligned. Where
/// they conflict the visible thing wins.
const TILE_EDGE_GAP: f32 = 5.;

/// Trailing inset for a group of tiles that ends on the window's right edge: the
/// glyph on [`CONTENT_INSET`] where there is room for it, never closer to the
/// edge than [`TILE_EDGE_GAP`].
pub(crate) fn tile_trailing_inset() -> f32 {
    (CONTENT_INSET - TILE_PAD).max(TILE_EDGE_GAP)
}

/// The same floor for the body-scale tiles inside a panel.
pub(crate) fn tile_trailing_inset_sm() -> f32 {
    (CONTENT_INSET - TILE_PAD_SM).max(TILE_EDGE_GAP)
}

/// What gpui-component's `TitleBar` already insets its content by, to clear the
/// window controls: 80px on macOS (traffic lights on the left), 12px elsewhere
/// (controls on the right). Anything laid out *inside* the bar therefore starts
/// here, not at the window edge.
pub(crate) const TITLE_BAR_LEAD: f32 = if cfg!(target_os = "macos") { 80. } else { 12. };

/// What the bar reserves at its *trailing* edge for the window controls: three
/// 34px tiles (─ ▢ ✕) off macOS, nothing on macOS (the traffic lights are on the
/// left, and `TITLE_BAR_LEAD` covers them). Anything in the bar that has to line
/// up with a column below it measures from the window edge minus this.
pub(crate) const WINDOW_CONTROLS_W: f32 = if cfg!(target_os = "macos") { 0. } else { 102. };

/// Left offset for the tile group that sits beside the window controls.
///
/// On macOS the thing that can collide with the traffic lights is the tile's
/// *hit box* — it paints a background on hover and when selected, [`TILE_PAD`]
/// wider than the glyph on each side — so this aligns the box, not the glyph, and the
/// bar's own 80px lead is already exactly the clearance macOS defines for that.
/// Hence zero: pulling back into the reserve to "hug" the lights only made the
/// hover capsule touch them. Off macOS the controls are on the right, nothing is
/// there to clear, and the group aligns its glyph to the content inset like the
/// rest of the chrome.
pub(crate) fn title_bar_hug_offset() -> f32 {
    if cfg!(target_os = "macos") {
        0.
    } else {
        tile_trailing_inset() - TITLE_BAR_LEAD
    }
}

/// Edge of the brand mark that anchors the window's leading corner off macOS
/// (see [`window_mark`]). Between a chrome tile's 32px hit box and its 13px
/// glyph: the mark paints no hover capsule, so what has to sit level with the
/// tiles beside it is its *ink* — and solid art reads heavier than line work at
/// equal size, hence short of the tile box rather than matching it.
pub(crate) const WINDOW_MARK_SIZE: f32 = 20.;

/// The "duo" mark — the same art the app icon and the About page carry — drawn
/// at the leading edge of the title-bar row, or `None` on macOS.
///
/// macOS owns that corner: the traffic lights sit there, and [`TITLE_BAR_LEAD`]
/// reserves them 80px. Everywhere else it is empty. The row's contents are the
/// rail's controls at its *right* end and the window chrome at the far side, so
/// the window's leading corner — the slot Windows reads as the app's identity,
/// filled by Explorer, VS Code and Zed alike — held nothing at all, which comes
/// across as unfinished rather than restrained.
///
/// Drawn, never clicked. It is not a menu button, so it stays out of the tile
/// rhythm (no hover capsule) and deliberately takes no `occlude()`: the row it
/// lives in is a `WindowControlArea::Drag`, and letting the mark fall through to
/// that keeps the strip grabbable instead of punching a dead 20px hole in it.
/// Make a row that stands in for the title bar behave like one: drag it to move
/// the window, double-click it to zoom.
///
/// Three rows do this. The rail's top zone sits level with the real bar but
/// outside it (the bar only spans the column beside the rail), and the code and
/// diff overlays each cover the bar with a header of their own drawn to its line.
/// Without this they are all dead strips: 40px across the top of the window that
/// look exactly like the caption and do nothing when you grab them.
///
/// Driven the way gpui-component's own `TitleBar` drives it — a press arms a
/// flag and the first *move* starts the window move — so a plain click, and a
/// double-click, still land intact. Note that on Windows the drag area maps to
/// HTCAPTION and the OS claims the press before gpui hit-tests, so every button
/// inside one of these rows needs an `occlude()` wrapper to get its clicks back.
pub(crate) fn title_bar_drag(row: gpui::Stateful<gpui::Div>) -> gpui::Stateful<gpui::Div> {
    let should_move = Rc::new(Cell::new(false));
    row.window_control_area(gpui::WindowControlArea::Drag)
        .on_mouse_down(gpui::MouseButton::Left, {
            let should_move = should_move.clone();
            move |_, _, _| should_move.set(true)
        })
        .on_mouse_up(gpui::MouseButton::Left, {
            let should_move = should_move.clone();
            move |_, _, _| should_move.set(false)
        })
        .on_mouse_move(move |_, window, _| {
            if should_move.replace(false) {
                window.start_window_move();
            }
        })
        .on_double_click(|_, window, _| {
            // gpui only implements `titlebar_double_click` on macOS — the trait
            // method is an empty default everywhere else, so on Linux this row
            // swallowed the double-click and nothing zoomed. `zoom_window` is the
            // maximise toggle there (x11 `_NET_WM_STATE_MAXIMIZED_*`, wayland
            // `set_maximized`), and what gpui-component's own `TitleBar` calls on
            // Linux for exactly this reason. Windows needs neither: the row is a
            // drag area, which maps to HTCAPTION, and the OS has already restored
            // or maximised the window before this could run.
            if cfg!(target_os = "linux") {
                window.zoom_window();
            } else {
                window.titlebar_double_click();
            }
        })
}

pub(crate) fn window_mark() -> Option<impl IntoElement> {
    if cfg!(target_os = "macos") {
        return None;
    }
    // Decoded once and shared: the title bar re-renders on every cursor blink,
    // and building a fresh `Image` per frame would re-copy the PNG and miss
    // gpui's image cache, which is keyed on the image's identity.
    static LOGO: std::sync::OnceLock<Arc<gpui::Image>> = std::sync::OnceLock::new();
    let logo = LOGO
        .get_or_init(|| {
            Arc::new(gpui::Image::from_bytes(
                gpui::ImageFormat::Png,
                include_bytes!("../../assets/logo@256.png").to_vec(),
            ))
        })
        .clone();
    Some(img(logo).size(px(WINDOW_MARK_SIZE)).flex_shrink_0())
}

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
    /// `Some` while this tab has the working-tree diff overlay open (clicked
    /// from a sidebar row's git line). Per-tab so switching away hides it and
    /// switching back restores it; closing the tab drops it. Only the active
    /// tab's overlay is rendered. See [`crate::ui::diff_overlay`].
    pub(crate) diff_overlay: Option<crate::ui::diff_overlay::DiffOverlayState>,
    /// This tab's code panel (file tree + editor overlay): open files, tree
    /// roots/expansion, and visibility. Same per-tab contract as
    /// `diff_overlay` — switching away hides it, switching back restores it,
    /// closing the tab drops it. Shared caches (directory listings, gitignore
    /// matchers, watchers) live on [`Tty7App`]. `None` until the panel is
    /// first opened in this tab.
    pub(crate) code: Option<Box<crate::ui::code_editor::TabCode>>,
    /// The sidebar group this tab last *definitively* belonged to: the
    /// repository home of its first pane's cwd — the main checkout's root, so
    /// linked worktrees of one repo share a group (deliberately not the
    /// focused pane's cwd — switching focus between splits must not relocate
    /// the row), or `None` for outside any repo (the "Scratch" group). Sticky
    /// on purpose: it only moves when
    /// the git cache has a landed answer for the current cwd
    /// ([`GitStatusCache::known_repo_for`](crate::terminal::git_status::GitStatusCache::known_repo_for)
    /// returns `Some`), so a cd whose probe is still in flight — or a pane
    /// with no cwd reported yet — keeps the row where it was instead of
    /// flickering through the Scratch group and back. A `RefCell` because the
    /// sidebar refreshes it during render, which only has `&Tab`.
    pub(crate) sidebar_group: std::cell::RefCell<Option<std::path::PathBuf>>,
    /// Which of the two full-column overlays (code panel, diff) was raised last.
    /// They deliberately have no fixed precedence: whichever the user just acted
    /// on paints on top, so opening a diff over the editor shows the diff, and
    /// clicking a file in the tree behind it brings the editor back — the same
    /// "click it, it comes forward" rule as window stacking.
    pub(crate) overlay_top: OverlayTop,
}

/// Stacking order for the two overlays that cover the whole column. See
/// [`Tab::overlay_top`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum OverlayTop {
    #[default]
    Code,
    Diff,
}

impl Tab {
    fn new(pane: Pane) -> Self {
        Self {
            pane,
            name: None,
            last_focused: None,
            diff_overlay: None,
            code: None,
            overlay_top: OverlayTop::default(),
            sidebar_group: std::cell::RefCell::new(None),
        }
    }

    /// The pane to focus when this tab becomes active: the last-focused leaf if
    /// it still exists, otherwise the first leaf.
    fn focus_target(&self) -> Option<crate::ui::pane::PaneSlot> {
        match self.last_focused {
            Some(id) => self.pane.leaf_matching_or_first(|l| l.entity_id() == id),
            None => self.pane.first_leaf(),
        }
    }

    /// The pane the right panel's detail should describe. Not simply the
    /// focused leaf: opening the panel, the diff overlay or the editor moves
    /// focus off the terminal entirely, and `focused_or_first` would then fall
    /// back to the *first* pane — so a split's second pane would silently swap
    /// the panel's cwd the moment you interacted with the panel. Falling back to
    /// `focus_target` uses the pane that held focus when it left instead, which
    /// is the one the user still thinks of as active.
    pub(crate) fn detail_pane(
        &self,
        window: &Window,
        cx: &gpui::App,
    ) -> Option<Entity<TerminalView>> {
        self.pane
            .focused_leaf(window, cx)
            .or_else(|| self.focus_target())
            .and_then(|slot| slot.terminal().cloned())
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
        leaf.and_then(|l| l.terminal().cloned())
            .map(|l| l.read(cx).title.clone())
            .unwrap_or_default()
    }

    /// The git snapshot (branch + working-tree diff) of the tab's label-driving
    /// terminal — the focused leaf with a `window`, else the first — for the
    /// sidebar row's branch line (the branch and change count shown under the
    /// title). Read through the shared per-repo cache, so tabs in one work
    /// tree always agree. `None` when that leaf isn't inside a git work tree,
    /// or before the repo's first probe lands.
    pub(crate) fn git_status(
        &self,
        window: Option<&Window>,
        cx: &App,
    ) -> Option<crate::terminal::git_status::GitStatus> {
        let leaf = match window {
            Some(window) => self.pane.focused_or_first(window, cx),
            None => self.pane.first_leaf().and_then(|s| s.terminal().cloned()),
        }?;
        leaf.read(cx).git_status(cx)
    }

    /// The coding agent running in this tab, or `None`. Any leaf counts (a
    /// split with a shell on the left and Claude on the right is an agent
    /// tab); the first agent leaf in tree order wins. Drives the tab avatar's
    /// brand mark.
    pub(crate) fn agent(&self, cx: &App) -> Option<crate::core::cli_agent::CLIAgent> {
        self.pane
            .terminals()
            .into_iter()
            .find_map(|l| l.read(cx).agent())
    }

    /// The tab's most urgent agent status across its leaves — waiting beats
    /// working beats done beats idle — or `None` when no leaf runs an agent.
    /// The green `Done` state always shows (a finished turn stays visible until
    /// the next one); [`agent_unread_count`](Self::agent_unread_count) then
    /// says how many of those finished turns are unread. Drives the avatar dot
    /// and the sidebar counts.
    pub(crate) fn agent_status(&self, cx: &App) -> Option<crate::core::cli_agent::AgentStatus> {
        use crate::core::cli_agent::AgentStatus;
        let urgency = |s: AgentStatus| match s {
            AgentStatus::Waiting => 3,
            AgentStatus::Working => 2,
            AgentStatus::Done => 1,
            AgentStatus::Idle => 0,
        };
        self.pane
            .terminals()
            .into_iter()
            .filter(|l| l.read(cx).agent().is_some())
            .map(|l| {
                l.read(cx)
                    .agent_session()
                    .map(|s| s.status)
                    .unwrap_or(AgentStatus::Idle)
            })
            .max_by_key(|s| urgency(*s))
    }

    /// How many of the tab's panes hold an *unread* finished turn — a `Done`
    /// the user hasn't looked at since. Drives the avatar dot's unread form:
    /// the green dot swells into a count badge (a split tab can finish several
    /// turns while you're away), and shrinks back to a plain dot once every
    /// pane has been seen. Zero when the shown status isn't `Done` — a busier
    /// pane (working/waiting) owns the corner until it settles.
    pub(crate) fn agent_unread_count(&self, cx: &App) -> usize {
        use crate::core::cli_agent::AgentStatus;
        if self.agent_status(cx) != Some(AgentStatus::Done) {
            return 0;
        }
        self.pane
            .terminals()
            .into_iter()
            .filter(|l| {
                let v = l.read(cx);
                v.agent_session().map(|s| s.status) == Some(AgentStatus::Done)
                    && v.agent_result_unread()
            })
            .count()
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

/// In-flight inline rename of the current workspace (the title-bar chip turns
/// into a text field). Mirrors [`Renaming`], but keyed to nothing — there is
/// only ever one current workspace per window.
pub(crate) struct WorkspaceRename {
    pub(crate) input: Entity<InputState>,
    _subs: Vec<Subscription>,
}

pub(crate) struct LoopbackForwardPanelState {
    /// The pane whose add/edit form is expanded under the Info tab's Forwards
    /// band, or `None` while the band is just its list. Per-pane rather than a
    /// bare flag so switching panes with a form open doesn't offer the new pane
    /// a form half-filled with the old one's values.
    pub(crate) form_pane_id: Option<u64>,
    /// The unified forwards list (Local/Remote/Dynamic, including auto localhost
    /// forwards) for the pane the Info tab is showing (WS4).
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
    /// Currently-applied terminal-emulator defaults. Tracked so hot-reload can
    /// push only the alacritty-backed options that actually changed.
    terminal_cursor_style: ConfigCursorStyle,
    terminal_scrollback_limit: usize,
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
    /// Keeps the `observe_global::<GitStatusCache>` subscription alive: a git
    /// probe landing (from *any* pane) repaints the sidebar, so every row in
    /// the same repo shows the just-refreshed branch/diff line, not a stale
    /// per-row copy. Never read.
    _git_status_watch: Subscription,
    /// Keeps the `observe_global::<PaneLivenessCache>` subscription alive: a
    /// machine's answer about which panes it still has lands on a background
    /// task, so nothing in this window would otherwise redraw the picker row or
    /// menu row that asked for it. Never read.
    _pane_liveness_watch: Subscription,
    /// Keeps the window-appearance observer alive: while
    /// `Config::theme_follow_system` is on, an OS light/dark flip re-resolves
    /// the theme slot and repaints. Never read.
    _appearance_watch: Subscription,
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
    /// `Some` while the "New Worktree Tab" sheet is open (see
    /// `ui::worktree_prompt`); `None` otherwise.
    pub(crate) worktree_prompt: Option<crate::ui::worktree_prompt::WorktreePrompt>,
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
    /// Right detail panel (info / changes / files) docked beside the terminal.
    pub(crate) right_panel: crate::ui::right_panel::RightPanelState,
    /// Local project file tree (left column of the body).
    pub(crate) file_tree: crate::ui::file_tree::FileTreeState,
    /// Code-editor panel (right column of the body).
    pub(crate) editor: crate::ui::code_editor::EditorPanelState,
    /// Vertical tab sidebar width (px), held in a shared `Cell` so the resize
    /// drag's window-level mouse listener can mutate it without the entity handle
    /// (mirrors the split divider's `ratio`). Seeded from `Config::sidebar_width`
    /// and persisted back when a drag ends.
    pub(crate) sidebar_width: Rc<Cell<f32>>,
    /// Whether the sidebar's resize handle is currently held.
    pub(crate) sidebar_dragging: Rc<Cell<bool>>,
    /// Right detail panel width (px) and drag state, held in shared `Cell`s for
    /// exactly the reason `sidebar_width` is — see there.
    pub(crate) right_panel_width: Rc<Cell<f32>>,
    pub(crate) right_panel_dragging: Rc<Cell<bool>>,
    /// Which chrome this *window* is showing: is the detail panel docked open,
    /// which of its tabs is selected, and is the tab rail collapsed.
    ///
    /// Window-level rather than `Config`, which is a global: with one window the
    /// two were indistinguishable, but with several, reading the config meant
    /// opening the detail panel in one window opened it in every other one too.
    /// A window is a *view* — what it has on screen is its own. The config
    /// fields of the same names survive as what a newly opened window starts
    /// with, written back on each toggle so a new window (and the next launch)
    /// inherits the last thing the user actually chose.
    pub(crate) right_panel_visible: bool,
    pub(crate) right_panel_tab: RightPanelTab,
    pub(crate) sidebar_collapsed: bool,
    /// Scroll handle for the sidebar's row list, so activating a tab scrolls its
    /// row into view — and so the rail's overlay scrollbar has an offset to
    /// track and drag (see [`crate::ui::scrollbar`]).
    pub(crate) sidebar_scroll: gpui::ScrollHandle,
    /// `Some` while a tab / group is being dragged to a new position, in either
    /// the strip or the rail: the frozen geometry the live preview reflow is
    /// computed against (see [`crate::ui::reorder`]). Shared by `Rc` because the
    /// `on_drag` that opens it only gets `&mut App`, not the entity. Cleared on
    /// the first frame after gpui ends the drag.
    pub(crate) reorder: Rc<RefCell<Option<crate::ui::reorder::Reorder>>>,
    /// Filter box in the sidebar's top control bar ("Search tabs…"); its text
    /// narrows the visible rows by fuzzy-ish substring match on the tab label.
    pub(crate) sidebar_search: Entity<InputState>,
    /// Live filter for the detail panel's Files tab (its own box, so filtering
    /// the tree never disturbs the tab list's filter and vice versa).
    pub(crate) file_search: Entity<InputState>,
    /// Re-renders the sidebar on each search keystroke so results narrow live.
    _sidebar_search_sub: Subscription,
    _file_search_sub: Subscription,
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
    /// Latest window geometry (the restore bounds while fullscreen), kept
    /// current by a bounds observer so the quit hook can persist it to
    /// `window.json` — at quit time no `&Window` is in reach to ask directly.
    window_bounds: Bounds<Pixels>,
    /// Which persistent workspace this window is showing. The window is the
    /// transient view; the workspace is the identity that survives closing it
    /// and shows up in the home-page picker. Every `save_session` writes back
    /// under this id, so two windows never overwrite each other's tabs.
    pub(crate) workspace: WorkspaceId,
    /// `Some` while the title-bar workspace chip is being renamed inline.
    /// Separate from `renaming` (tabs) because the two live in different
    /// widgets and can't be in flight at once anyway.
    pub(crate) workspace_rename: Option<WorkspaceRename>,
    /// Last title pushed to the OS window, so the common case (nothing
    /// changed) skips the platform call. `RefCell` because the sync runs from
    /// `focus_active`, which only takes `&self`.
    window_title: std::cell::RefCell<String>,
    /// The home page's "Connect to Host" flow, or `None` when it isn't running
    /// (design §10). Lives on the window rather than on the app because a
    /// window is what a remote workspace ends up bound to — two windows can be
    /// reaching two different machines at once.
    pub(crate) connect: Option<crate::ui::remote_workspace::ConnectFlow>,
    /// The workspace switcher overlay, or `None` when it is closed.
    pub(crate) switcher: Option<crate::ui::switcher::Switcher>,
    /// What each machine's handshake reported, kept past the connect flow so
    /// the switcher can show several connected machines at once (the flow only
    /// ever holds one).
    pub(crate) host_snapshots: std::collections::HashMap<
        crate::ui::host_registry::HostId,
        crate::ui::switcher::HostSnapshot,
    >,
}

/// Which close action a live-SSH close-confirmation is gating (PRD FR-E3).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SshCloseKind {
    /// Close the whole tab at this index.
    Tab(usize),
    /// Close the focused pane.
    Pane,
}

/// Where a forked agent session lands. The placement is not a preference but a
/// consequence of *where the user asked from* (issue #211): a pane-level ask is
/// spatial, so the pane menu offers the four directions; a tab-level ask is
/// not, so the tab menu — and the bare action behind the palette / menu bar —
/// opens a new tab with no placement question.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ForkPlacement {
    NewTab,
    /// Split the source pane along `axis`, the fork taking the second slot —
    /// or the first when `before`, which is Split Left / Split Up.
    Split {
        axis: Axis,
        before: bool,
    },
}

/// What the agent-session menu rows need to know about a tab, read at
/// menu-open time (like `tab_cwd`) so enablement can't go stale between render
/// and click.
pub(crate) struct TabAgentSession {
    /// The fork row's label, or `None` when tty7 has no verified fork command
    /// for this agent — then no fork row is offered at all, rather than a
    /// disabled one promising a capability that doesn't exist.
    pub(crate) fork_label: Option<&'static str>,
    /// The agent's native session id, absent until its hooks report one.
    pub(crate) session_id: Option<String>,
    /// A remote pane. Forking shells a *local* agent binary, which would branch
    /// the wrong machine's session, so the row disables there.
    pub(crate) remote: bool,
}

impl TabAgentSession {
    /// Whether a fork can actually run right now: the agent has a fork command,
    /// tty7 has seen its session id, and the pane is local.
    pub(crate) fn forkable(&self) -> bool {
        self.fork_label.is_some() && self.session_id.is_some() && !self.remote
    }
}

impl Tty7App {
    /// A window on `id`'s workspace — reopening one from the picker — or on a
    /// fresh workspace when `id` is `None` (New Workspace) or names a workspace
    /// that is no longer on file.
    pub fn for_workspace(
        id: Option<WorkspaceId>,
        fresh: crate::ui::windows::FreshStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Claiming marks the workspace open and hands back its saved tabs, so
        // the store (not this window) stays the single writer of session.json.
        let restore = cx.global::<Config>().restore_session;
        let known = id.is_some_and(|id| WorkspaceStore::all(cx).get(id).is_some());
        let (workspace, saved) = WorkspaceStore::claim(cx, id);
        // A workspace that was already on file restores its tab/split layout and
        // each pane's cwd, unless the user turned restore off — then it starts
        // fresh. A *brand-new* one has no tabs to restore, so what it comes up
        // with is the caller's call: `None` here takes the first-run path in
        // `with_session`, spawning a single default terminal, which is what
        // `New Workspace` and a first run both want. Handing an empty session
        // through instead lands on the home page, for the launch that exists to
        // show the workspace picker.
        let session = match (known, fresh) {
            (true, _) => restore.then_some(saved),
            (false, crate::ui::windows::FreshStart::Shell) => None,
            (false, crate::ui::windows::FreshStart::HomePage) => Some(Session::default()),
        };
        let app = Self::with_session(Some(workspace), session, window, cx);
        // Persist right away. The leaves just spawned (or reattached) now carry
        // daemon pane ids, and nothing else writes them until the next
        // *structural* change — so a crash before the user happens to open a
        // tab would strand every one of those panes in the daemon.
        app.save_session(cx);
        // If startup reused a daemon that speaks a different wire protocol
        // (an app upgrade while the old service kept running), the sessions
        // just restored above are living on that old dialect. Surface the
        // keep-or-restart choice now that there's a window to ask in.
        Self::prompt_daemon_version_mismatch(window, cx);
        // The same question for any *remote* server this client already found
        // at a different build, and the consent handler that the install path
        // asks before writing a binary onto someone else's machine (design §12).
        crate::ui::remote_connect::register(cx);
        Self::prompt_remote_daemon_mismatch(window, cx);
        // A window that came back on a remote workspace has its last-pulled
        // layout but no connection yet. Reconnecting is M6's; this is the seam
        // it hooks into, and the reason nothing on the launch path had to learn
        // whether a workspace is local.
        app.reopen_remote_at_startup(cx);
        app
    }

    /// Ask what to do about a protocol-mismatched daemon that
    /// `spawn::ensure_running` deliberately left running (rather than silently
    /// killing every persisted session at startup): keep using it — sessions
    /// survive, features whose wire shape changed may misbehave — or restart
    /// the service clean via the shared
    /// [`restart_daemon_confirmed`](Self::restart_daemon_confirmed) path
    /// (tabs reopen with fresh shells). Keeping is the default: dismissing
    /// the prompt changes nothing.
    fn prompt_daemon_version_mismatch(window: &mut Window, cx: &mut Context<Self>) {
        let Some(mismatch) = crate::daemon::spawn::take_mismatched_daemon() else {
            return;
        };
        let ours = crate::daemon::protocol::PROTOCOL_VERSION;
        let detail = match mismatch.version {
            Some(v) => format!(
                "The daemon holding your sessions is from another build \
                 (v{}, protocol {} — this app speaks {}). You can keep using it and \
                 your sessions stay, but features whose wire format changed may \
                 misbehave until it's restarted. Restarting starts a clean daemon: \
                 tabs reopen with fresh shells and anything running in them is \
                 terminated.",
                v.build, v.protocol, ours
            ),
            None => "The daemon holding your sessions is from an older \
                 version of the app. You can keep using it and your sessions stay, \
                 but newer features may misbehave until it's restarted. Restarting \
                 starts a clean daemon: tabs reopen with fresh shells and anything \
                 running in them is terminated."
                .to_string(),
        };
        // Phrased as the question it is, like every other prompt in the app —
        // this one used to be a bare statement of fact with two verbs under it.
        // The version details it used to carry in the title are in the body.
        let answer = window.prompt(
            PromptLevel::Warning,
            "Restart Daemon?",
            Some(&detail),
            &["Keep Sessions", "Restart"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            // Index 1 == "Restart"; "Keep Sessions" or a dismissed prompt leave
            // the old daemon (and every session) untouched.
            if !matches!(answer.await, Ok(1)) {
                return;
            }
            let _ = this.update_in(cx, |this, _window, cx| this.restart_daemon_confirmed(cx));
        })
        .detach();
    }

    /// The whole constructor behind `new`, with the saved session injected
    /// instead of read from disk. The headless tests build the app through
    /// this seam (a zero-tab session → the home page, no terminal spawned)
    /// so every subscription and window hook runs exactly as in production
    /// without touching `~/.config` or a daemon.
    pub(crate) fn with_session(
        workspace: Option<WorkspaceId>,
        session: Option<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Tests build a window without going through the store; give them a
        // detached identity rather than requiring the global to be installed.
        let workspace = workspace.unwrap_or_default();
        // The route every pane this window opens will take. Resolved once, here,
        // rather than per pane: the window's machine cannot change under it, and
        // a per-pane lookup is a per-pane chance to disagree. `None` for a local
        // workspace, which is every window that existed before M5.
        let pane_ws = crate::ui::remote_workspace::pane_workspace_for(cx, workspace);
        // Font size from config (borrow ends before the mutable theme apply).
        let (
            font_size,
            line_height,
            font_family,
            font_family_bold,
            font_family_italic,
            font_features,
            terminal_cursor_style,
            terminal_scrollback_limit,
        ) = {
            let cfg = cx.global::<Config>();
            (
                cfg.font_size,
                cfg.line_height,
                cfg.font_family.clone(),
                cfg.font_family_bold.clone(),
                cfg.font_family_italic.clone(),
                cfg.font_features
                    .as_ref()
                    .map(crate::core::config::gpui_font_features),
                cfg.cursor_style,
                cfg.scrollback_limit,
            )
        };
        let sftp_panel = crate::ui::sftp::SftpPanelState::new(window, cx);
        let file_tree = crate::ui::file_tree::FileTreeState::new(window, cx);
        let editor = crate::ui::code_editor::EditorPanelState::new(window, cx);
        // Managed-forward add-form inputs (native-SSH panes).
        let mf_bind_host = cx.new(|cx| InputState::new(window, cx).default_value("127.0.0.1"));
        let mf_bind_port = cx.new(|cx| InputState::new(window, cx).placeholder("8080"));
        let mf_target_host = cx.new(|cx| InputState::new(window, cx).placeholder("127.0.0.1"));
        let mf_target_port = cx.new(|cx| InputState::new(window, cx).placeholder("80"));
        let mf_description = cx.new(|cx| InputState::new(window, cx).placeholder("description"));
        let sidebar_width = cx.global::<Config>().sidebar_width;
        let right_panel_width = cx.global::<Config>().right_panel_width;
        // The config's copies are this window's *starting* chrome; from here on
        // the window owns them (see the fields' doc comment).
        let right_panel_visible = cx.global::<Config>().right_panel_visible;
        let right_panel_tab = cx.global::<Config>().right_panel_tab;
        let sidebar_collapsed = cx.global::<Config>().sidebar_collapsed;
        // Live-apply hot-reloaded config: the watcher in `main.rs` swaps the
        // `Config` global on every `config.json` change, which fires this. The
        // window-aware variant so the reload can re-run `apply_theme` with the
        // window (blur flip, traffic-light pinning) and re-sync the Appearance
        // opacity slider — the watcher task itself has no window handle.
        let config_watch = cx.observe_global_in::<Config>(window, |this, window, cx| {
            this.reload_from_config(window, cx)
        });
        // Repaint when any pane's git probe lands in the shared cache — the
        // sidebar's branch/diff lines read from it, and the probing pane's own
        // notify wouldn't re-render rows belonging to *other* panes. The open
        // diff overlay rides the same signal: if the landed numbers disagree
        // with what it shows, it re-probes the full diff.
        cx.default_global::<crate::terminal::git_status::GitStatusCache>();
        let git_status_watch =
            cx.observe_global::<crate::terminal::git_status::GitStatusCache>(|this, cx| {
                this.maybe_refresh_diff_overlay(cx);
                // Same trigger, same freshness: the right panel's Changes list is
                // the sidebar's `+N −M` expanded, so it re-probes whenever those
                // numbers do rather than going stale behind them.
                this.right_panel_refresh_changes(cx);
                cx.notify();
            });
        // The same shape for pane liveness: the picker and the workspace menu
        // read a per-machine cache that is filled off the UI thread, so the
        // frame that *asked* is long gone by the time an answer lands and
        // nothing else would repaint it.
        cx.default_global::<crate::terminal::pane_liveness::PaneLivenessCache>();
        let pane_liveness_watch = cx
            .observe_global::<crate::terminal::pane_liveness::PaneLivenessCache>(|_this, cx| {
                cx.notify();
            });
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
        let activation_watch = cx.observe_window_activation(window, |this, window, cx| {
            this.dismiss_mod_hint(cx);
            // The panes' link-modifier tracking loses the release the same
            // way, and a stale "⌘ held" is worse than missing badges: a
            // plain unmodified click would open links. Treat the flip as a
            // release; holding ⌘ again re-arms it via `on_modifiers_changed`.
            this.set_link_modifier(false, cx);
            // Coming back is the only cue we get that the working tree may
            // have moved while the user was elsewhere: an edit in another
            // editor, a `git` command in another app, an agent in another
            // window. None of those reach a pane's poll loop, so without this
            // the sidebar's `+N −N` would keep showing pre-alt-tab numbers
            // until the user happened to run a command in the pane.
            if window.is_window_active() {
                // Whichever window the user last brought forward is the one to
                // focus on the next launch — `claim` only ever records the
                // *last opened* workspace, which is a different thing.
                WorkspaceStore::focus(cx, this.workspace);
                this.refresh_git_status_all(cx);
            }
        });
        // Follow OS light/dark flips live: while "sync with system" is on, an
        // appearance change re-resolves the theme slot and repaints. While it's
        // off the appearance only ever changes because `apply_theme` pinned it
        // to the theme — skip, or the pin would re-trigger a redundant apply.
        let this = cx.weak_entity();
        let appearance_watch = window.observe_window_appearance(move |window, cx| {
            // This is the only place the OS flip is observed, so cache it here —
            // from the *window*, never `cx.window_appearance()`, which would
            // re-enter gpui's already-borrowed Linux client and panic. Before the
            // early return, so a flip that happens while following is off still
            // lands. See `ui::theme::SystemAppearance`.
            crate::ui::theme::note_system_appearance(window, cx);
            if !cx.global::<Config>().theme_follow_system {
                return;
            }
            apply_theme(Some(window), cx);
            let _ = this.update(cx, |this, cx| {
                // The editor targets the on-screen theme, and with no global
                // override the opacity slider follows it — keep both in step.
                this.rebuild_theme_editor(window, cx);
                this.sync_window_opacity_slider(window, cx);
                cx.notify();
            });
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
            None => match new_terminal(pane_ws.clone(), font_size, None, None, None, window, cx) {
                Ok(first) => (vec![Tab::new(Pane::leaf(first))], 0),
                // The daemon we just tried to start isn't answering. A window
                // with no tabs is a legal state (it shows the home page), and
                // far better than taking the launch down over it.
                Err(e) => {
                    log::error!("first terminal failed to start: {e}");
                    (Vec::new(), 0)
                }
            },
            // A saved session (with tabs, or an empty home-page state): rebuild it
            // the same way a daemon restart does.
            some => tabs_from_session(pane_ws.as_ref(), some, font_size, window, cx),
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
        let file_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search files…"));
        let file_search_sub = cx.subscribe_in(&file_search, window, |_this, _i, ev, _w, cx| {
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
            terminal_cursor_style,
            terminal_scrollback_limit,
            _config_watch: config_watch,
            _keystroke_watch: keystroke_watch,
            _activation_watch: activation_watch,
            _git_status_watch: git_status_watch,
            _pane_liveness_watch: pane_liveness_watch,
            _appearance_watch: appearance_watch,
            palette: None,
            palette_sub: None,
            closed: Vec::new(),
            renaming: None,
            worktree_prompt: None,
            maximized: None,
            mod_hint_badges: false,
            mod_hint_gen: 0,
            record_gen: 0,
            home_focus: cx.focus_handle(),
            detected_shells: Vec::new(),
            loopback_panel: LoopbackForwardPanelState {
                form_pane_id: None,
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
            right_panel: Default::default(),
            file_tree,
            editor,
            sidebar_width: Rc::new(Cell::new(sidebar_width)),
            sidebar_dragging: Rc::new(Cell::new(false)),
            right_panel_width: Rc::new(Cell::new(right_panel_width)),
            right_panel_dragging: Rc::new(Cell::new(false)),
            right_panel_visible,
            right_panel_tab,
            sidebar_collapsed,
            sidebar_scroll: gpui::ScrollHandle::new(),
            reorder: Rc::new(RefCell::new(None)),
            sidebar_search,
            _sidebar_search_sub: sidebar_search_sub,
            file_search,
            _file_search_sub: file_search_sub,
            settings: None,
            ssh_prompt: crate::ui::ssh_prompt::SshPromptState::new(cx),
            ssh_close_confirm: None,
            window_bounds: window.window_bounds().get_bounds(),
            workspace,
            workspace_rename: None,
            window_title: std::cell::RefCell::new(String::new()),
            connect: None,
            switcher: None,
            host_snapshots: std::collections::HashMap::new(),
        };
        // Bring the system tray up (icon + agent menu + poll loop) — but only
        // for the *first* window: the tray is one app-wide icon, and letting
        // every window register its own would stack N icons in the status bar.
        // `register` happens after `open_window` returns, so during the first
        // window's construction the registry is still empty.
        //
        // Skipped in tests: the headless harness has no native status bar to
        // register with, and the poll task would just spin against the mocked
        // clock.
        if !cfg!(test) && crate::ui::windows::WindowRegistry::count(cx) == 0 {
            crate::ui::tray::init(cx);
        }
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
            // Also persist the window's final geometry so the next launch can
            // reopen there (`remember_window_size`). Written unconditionally —
            // startup gates on the config — so toggling the setting back on
            // restores the most recent quit, not some stale pre-toggle state.
            crate::core::window_state::WindowState::from_bounds(app.window_bounds).save();
            async move {}
        })
        .detach();

        // Keep `window_bounds` tracking the live window: moves and resizes both
        // fire this observer, and `window_bounds()` reports the *restore* bounds
        // while fullscreen, so a fullscreen quit doesn't record a screen-sized
        // window for the next normal launch.
        cx.observe_window_bounds(window, |this, window, _cx| {
            this.window_bounds = window.window_bounds().get_bounds();
        })
        .detach();

        // Closing a window *detaches* its workspace: the panes keep running in
        // the daemon, and the workspace drops into the home-page picker to be
        // reopened later. So closing one of several windows is cheap and needs
        // no confirmation — the user can see the others and get this one back.
        //
        // The last window is different: closing it also quits the app (a
        // windowless process left in the Dock no longer responds to being
        // clicked — #147), so that one keeps the reassuring prompt by default.
        // We veto the immediate close (return `false`), show it, and quit only
        // if the user picks "Close"; a one-shot flag lets that post-confirm
        // close through instead of looping the prompt.
        //
        // `confirm_window_close` turns the prompt off for users who have learned
        // the model — it is teaching, not protection (⌘Q never asked), so it has
        // to be escapable.
        let close_confirmed = std::rc::Rc::new(std::cell::Cell::new(false));
        let weak_app = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            if close_confirmed.get() {
                return true;
            }
            let last_window = crate::ui::windows::WindowRegistry::count(cx) <= 1;
            let empty = weak_app
                .upgrade()
                .is_some_and(|app| app.read(cx).tabs.is_empty());

            // Any window but the last, an empty one with nothing to reassure
            // about, or a user who has turned the prompt off: detach and go.
            // Prompting here would be friction.
            let confirm = cx.global::<Config>().confirm_window_close;
            if !last_window || empty || !confirm {
                if let Some(app) = weak_app.upgrade() {
                    app.update(cx, |app, cx| app.detach_workspace(cx));
                }
                if last_window {
                    // Deferred onto the next tick so the close itself completes
                    // first, same as the confirmed path below.
                    cx.spawn(async move |cx| {
                        let _ = cx.update(|cx| cx.quit());
                    })
                    .detach();
                }
                return true;
            }

            let answer = window.prompt(
                PromptLevel::Info,
                "Close Window?",
                // What this promises has to match what the next launch does.
                // Closing the last window *detaches* its workspace rather than
                // ending it: the panes keep running in the daemon, but tty7
                // comes back on the home page with the workspace waiting in the
                // picker — it no longer reopens it unasked, so promising it
                // would be restored would be a promise the app doesn't keep.
                //
                // Points at the title bar's workspace menu, not the macOS
                // Window menu: there is no menu bar on Windows or Linux, and
                // the corner chip is the one place that lists workspaces on
                // every platform.
                Some(
                    "Your sessions keep running in the background. This \
                     workspace will be waiting on the home page, and in the \
                     workspace menu in the title bar, the next time you open \
                     tty7.",
                ),
                &["Cancel", "Close"],
                cx,
            );
            let close_confirmed = close_confirmed.clone();
            let weak_app = weak_app.clone();
            cx.spawn(async move |cx| {
                // Index 1 == "Close"; index 0 (Cancel) and a dismissed prompt
                // both leave the window open.
                if let Ok(1) = answer.await {
                    close_confirmed.set(true);
                    let _ = cx.update(|cx| {
                        if let Some(app) = weak_app.upgrade() {
                            app.update(cx, |app, cx| app.detach_workspace(cx));
                        }
                        cx.quit();
                    });
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
    pub(crate) fn save_session(&self, cx: &mut App) {
        let tabs: Vec<SessionTab> = self
            .tabs
            .iter()
            .map(|tab| tab_to_session(tab, cx))
            .collect();
        // Zero tabs is a real state (the home page) and is persisted as such, so
        // the next launch comes back to it instead of a fresh shell.
        let active = if tabs.is_empty() {
            0
        } else {
            self.active.min(tabs.len() - 1)
        };
        let session = Session { active, tabs };
        // The store merges this into the other windows' workspaces and owns the
        // write; the geometry rides along so reopening lands where we are now.
        WorkspaceStore::record(
            cx,
            self.workspace,
            session,
            Some(WindowState::from_bounds(self.window_bounds)),
        );
        // …and for a remote workspace the machine that owns the layout has to
        // hear about it, or `session.json` is the only place it exists and any
        // other client (or a fresh install) opens the workspace empty. No-ops
        // for a local workspace and for a machine we are not connected to —
        // the latter is also what keeps a window that failed to restore from
        // pushing its emptiness over a good record.
        self.push_remote_layout(self.workspace, cx);
    }

    /// This window is going away: capture its final state (a plain `cd` may
    /// have moved a pane's cwd with no structural change to trigger a save),
    /// mark the workspace closed so the home-page picker lists it, and drop it
    /// from the registry so "is this the last window?" stays accurate.
    ///
    /// A *detach*, not a teardown — the daemon panes keep running and reattach
    /// when the workspace is reopened.
    pub(crate) fn detach_workspace(&self, cx: &mut App) {
        self.save_session(cx);
        // An empty workspace has nothing to come back to, so it is dropped
        // outright instead of accumulating as a blank row in the picker —
        // every `New Workspace` the user closes without using would leave one.
        //
        // Unless the emptiness is *this client's* ignorance rather than the
        // machine's answer. `claimable_session` deliberately opens a remote
        // workspace empty when its machine cannot be reached, so a window
        // opened while the box was asleep and then closed — there was nothing
        // in it to work on — would take the entry with it: its `RemoteRef`, its
        // cached layout and its geometry, while its panes are still running
        // over there. Nothing would reconnect it and nothing would offer it
        // again; the only way back is re-adding the machine by hand.
        let answered = WorkspaceStore::machine_is_connected(cx, self.workspace);
        if self.tabs.is_empty() && answered {
            WorkspaceStore::remove(cx, self.workspace);
        } else {
            WorkspaceStore::close_window(cx, self.workspace);
        }
        crate::ui::windows::WindowRegistry::unregister(cx, self.workspace);
        // The workspace just moved from "on screen" to "detached" — the Window
        // menu is the only place that says so.
        crate::ui::windows::refresh_menu(cx);
    }

    /// Design §15's other half: a workspace's forwards belong to the workspace,
    /// so stopping it has to end them — nothing else will. A pane's forwards
    /// need no equivalent; the daemon drops those with the pane.
    ///
    /// Best effort and silent. The window is on its way out either way, and a
    /// forward that could not be torn down (the connection already dropped) is
    /// already gone with the connection that carried it.
    pub(crate) fn teardown_workspace_forwards(&self, cx: &gpui::App) {
        let Some(route) = self
            .tabs
            .iter()
            .flat_map(|tab| tab.pane.terminals())
            .find_map(|leaf| {
                let view = leaf.read(cx);
                let workspace = view.workspace().cloned()?;
                Some(ForwardRoute {
                    pane_id: view.pane_id,
                    workspace: Some(workspace),
                })
            })
        else {
            return;
        };
        // Off the UI thread. `teardown` dials the daemon, which resolves the
        // workspace's SSH connection and waits for the server to acknowledge a
        // `cancel_tcpip_forward` — on a machine that has gone unreachable, which
        // is exactly when someone reaches for Stop Workspace, that never comes
        // back inside the request timeout. `ForwardRoute::list` is already
        // backgrounded for the same reason; this was the one that was not, and
        // it ran while the window was being torn down.
        cx.background_executor()
            .spawn(async move {
                let left = route.teardown();
                if !left.is_empty() {
                    log::warn!("{} forwards survived a workspace teardown", left.len());
                }
            })
            .detach();
    }

    /// Stop a workspace — kill its sessions and close its window — confirming
    /// first when something is still running. Its layout stays on file, so it
    /// can be started again later.
    ///
    /// Deliberately not called "close": the red traffic light closes a window
    /// and only detaches, while this ends the shells. Two actions that sit near
    /// each other need two different verbs, or the menu reads as if they were
    /// variations on one thing.
    pub(crate) fn stop_workspace(
        &mut self,
        id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::ui::windows::confirm_and_stop(cx, window, id);
        cx.notify();
    }

    /// Delete a workspace: stop it *and* discard the saved layout.
    pub(crate) fn delete_workspace(
        &mut self,
        id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::ui::windows::confirm_and_delete(cx, window, id);
        cx.notify();
    }

    /// Show the workspace in the Window menu's slot `index`. A stale slot (the
    /// menu was built before a workspace was stopped) is a no-op rather than an
    /// error — the menu is rebuilt right after any such change anyway.
    pub(crate) fn select_workspace_slot(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((id, _open)) = crate::ui::windows::menu_order(cx).get(index).copied() else {
            return;
        };
        self.reveal_workspace(id, window, cx);
    }

    /// Show `id`'s workspace.
    ///
    /// One workspace is shown by exactly one window, so this either focuses the
    /// window it already has or opens a new one for it — never swaps it into
    /// *this* window, which would leave the workspace already here without one.
    ///
    /// The single exception is a window that is empty (the home page): reusing
    /// it beats opening a second window and stranding a blank frame, and there
    /// is no workspace to displace.
    pub(crate) fn reveal_workspace(
        &mut self,
        id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(handle) = crate::ui::windows::WindowRegistry::window_for(cx, id) {
            let _ = handle.update(cx, |_, other, _| other.activate_window());
            return;
        }
        if self.tabs.is_empty() {
            self.switch_workspace(id, window, cx);
        } else {
            crate::ui::windows::open(cx, Some(id));
        }
    }

    /// Swap this window over to `id`'s workspace in place, rebuilding its tabs.
    ///
    /// This is what the home-page picker does: the window running it is empty
    /// (the picker only shows on the home page), so opening a *second* window
    /// would strand this blank one. The outgoing workspace is dropped rather
    /// than detached for the same reason.
    pub(crate) fn switch_workspace(
        &mut self,
        id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self.workspace;
        if previous == id {
            return;
        }
        if self.tabs.is_empty() {
            WorkspaceStore::remove(cx, previous);
        } else {
            self.save_session(cx);
            WorkspaceStore::close_window(cx, previous);
        }

        let (claimed, session) = WorkspaceStore::claim(cx, Some(id));
        crate::ui::windows::WindowRegistry::rebind(cx, previous, claimed);
        self.adopt_workspace(claimed, session, window, cx);
    }

    /// Take over an *already claimed* workspace: rebuild this window's tabs
    /// from `session` and retitle it. Split from [`Self::switch_workspace`]
    /// because `ui::windows::close_workspace` gets here having already
    /// destroyed the outgoing workspace — there is nothing left to detach.
    pub(crate) fn adopt_workspace(
        &mut self,
        id: WorkspaceId,
        session: Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_host = self.spawn_host(cx);
        self.workspace = id;
        // The closed-tab stack is per *window* and survives a workspace swap,
        // so it is the one thing that could carry a tab across machines.
        self.rebind_host(previous_host, cx);
        let font_size = self.font_size;
        let pane_ws = self.window_workspace(cx);
        let (tabs, active) =
            tabs_from_session(pane_ws.as_ref(), Some(session), font_size, window, cx);
        self.tabs = tabs;
        self.active = active;
        self.maximized = None;
        // Same reason as `for_workspace`: capture the reattached/spawned pane
        // ids now rather than waiting for a structural change.
        self.save_session(cx);
        crate::ui::windows::refresh_menu(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Reopen the most recently closed tab (Cmd+Shift+T). Rebuilds its pane
    /// tree (restoring each terminal's saved cwd), inserts it after the active
    /// tab, and focuses it. No-op when the stack is empty.
    fn reopen_closed_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(st) = self.closed.pop() else {
            return;
        };
        let pane_ws = self.window_workspace(cx);
        let alive = alive_panes_on(&crate::terminal::PaneRoute::for_workspace(pane_ws.as_ref()));
        let Some(pane) = session_to_pane(
            pane_ws.as_ref(),
            &st.pane,
            &alive,
            self.font_size,
            window,
            cx,
        ) else {
            // Nothing came back (an unreachable daemon). Put the entry back so
            // the tab is still reopenable once the daemon is up again.
            window.push_notification("Could not reopen the tab: no terminal started", cx);
            self.closed.push(st);
            return;
        };
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
                diff_overlay: None,
                code: None,
                overlay_top: OverlayTop::default(),
                // Keep the group it had when closed — the row reappears where
                // it lived instead of flashing through Scratch.
                sidebar_group: std::cell::RefCell::new(st.sidebar_group),
            },
        );
        self.active = insert_at;
        self.focus_active(window, cx);
        self.save_session(cx);
        cx.notify();
    }

    // ── System tray (`ui::tray`) ────────────────────────────────────────────

    /// Whether this window hosts the pane with `leaf_id`. The tray's reveal
    /// carries a gpui entity id, which is unique app-wide, so this is how a
    /// click finds the one window that can act on it.
    pub(crate) fn owns_leaf(&self, leaf_id: u64) -> bool {
        self.tabs.iter().any(|t| {
            t.pane
                .leaves()
                .iter()
                .any(|l| l.entity_id().as_u64() == leaf_id)
        })
    }

    /// This window's agent panes, unsorted: brand name, status, and a "where"
    /// line (cwd directory name + git branch). Unsorted because the tray is a
    /// single icon for the whole app — it concatenates every window's rows and
    /// sorts once, most urgent first, so the pane that needs the user tops the
    /// menu.
    pub(crate) fn agent_rows(&self, cx: &App) -> Vec<crate::ui::tray::AgentRow> {
        use crate::core::cli_agent::AgentStatus;
        let mut agents = Vec::new();
        for tab in &self.tabs {
            for leaf in tab.pane.terminals() {
                let view = leaf.read(cx);
                let Some(agent) = view.agent() else { continue };
                let status = view
                    .agent_session()
                    .map(|s| s.status)
                    .unwrap_or(AgentStatus::Idle);
                let dir = view
                    .cwd()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
                let branch = view.git_status(cx).map(|g| g.branch);
                let detail = match (dir, branch) {
                    (Some(dir), Some(branch)) => format!("{dir} @ {branch}"),
                    (Some(dir), None) => dir,
                    // No cwd yet (pane still spawning) — the agent name alone
                    // still identifies the row.
                    (None, _) => String::new(),
                };
                agents.push(crate::ui::tray::AgentRow {
                    leaf_id: leaf.entity_id().as_u64(),
                    agent,
                    status,
                    detail,
                });
            }
        }
        agents
    }

    /// Apply a tray menu click. Runs on the foreground executor with the
    /// window in hand (see `tray::init`'s action pump).
    pub(crate) fn handle_tray_action(
        &mut self,
        action: crate::ui::tray::TrayAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::ui::tray::TrayAction;
        // Tray clicks arrive while another app is frontmost — that's the
        // tray's whole premise. `activate_window` alone only orders our
        // window front within the app (macOS: `makeKeyAndOrderFront:`); the
        // *application* must also be activated or the reveal — and any
        // window-modal prompt we show next — stays buried behind the app the
        // user clicked from.
        fn surface_window(window: &mut Window, cx: &mut App) {
            cx.activate(true);
            window.activate_window();
        }
        match action {
            TrayAction::ShowWindow => surface_window(window, cx),
            TrayAction::RevealPane { leaf_id } => {
                // Resolve the leaf against the *live* tree — the menu the user
                // clicked may predate a tab close; a vanished pane is a no-op
                // (the window still comes forward).
                let tab_ix = self.tabs.iter().position(|t| {
                    t.pane
                        .leaves()
                        .iter()
                        .any(|l| l.entity_id().as_u64() == leaf_id)
                });
                if let Some(ix) = tab_ix {
                    self.activate(ix, window, cx);
                    // The reveal must actually show the pane: a sibling leaf
                    // maximized in this tab would otherwise keep the target
                    // off-screen while we hand it keyboard focus. The target
                    // itself staying maximized is fine — it's already the
                    // visible one.
                    if self
                        .maximized
                        .as_ref()
                        .is_some_and(|m| m.entity_id().as_u64() != leaf_id)
                    {
                        self.maximized = None;
                    }
                    if let Some(leaf) = self.tabs[ix]
                        .pane
                        .leaves()
                        .into_iter()
                        .find(|l| l.entity_id().as_u64() == leaf_id)
                    {
                        self.tabs[ix].last_focused = Some(leaf.entity_id());
                        self.focus_leaf(&leaf, window, cx);
                    }
                    cx.notify();
                }
                surface_window(window, cx);
            }
            TrayAction::SetNotifyMode(mode) => self.set_notify_mode(mode, cx),
            TrayAction::OpenSettings => {
                surface_window(window, cx);
                if self.settings.is_none() {
                    self.toggle_settings(window, cx);
                }
            }
            TrayAction::CheckForUpdates => {
                surface_window(window, cx);
                // Same path as the App menu's "Check for Updates…" — the tray
                // used to carry its own copy of this, and was for a while the
                // only place in the app offering the check at all.
                self.check_for_updates_now(window, cx);
            }
            // Same as ⌘Q: sessions keep running in the daemon.
            TrayAction::Quit => cx.quit(),
            TrayAction::QuitStopSessions => self.quit_stop_sessions(window, cx),
        }
    }

    /// Tray "Quit and Stop Daemon": confirm, shut the daemon down (which
    /// hangs up every shell — the whole point of picking this over plain
    /// quit), then quit. The stop runs off the UI thread; like
    /// `--stop-daemon` it can take a beat while children get their grace
    /// period.
    fn quit_stop_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The prompt is window-modal and the click came from the tray with
        // another app frontmost — activate the app AND the window, or the
        // user never sees the question.
        cx.activate(true);
        window.activate_window();
        let answer = window.prompt(
            PromptLevel::Warning,
            "Quit and Stop Daemon?",
            Some(
                "This quits tty7 and stops the background daemon — anything \
                 still running in your sessions is terminated. Your tabs and \
                 layout are kept and reopen with fresh shells next launch. \
                 (Plain Quit keeps sessions running.)",
            ),
            &["Cancel", "Quit and Stop"],
            cx,
        );
        cx.spawn(async move |_this, cx| {
            // Index 1 == "Quit and Stop"; Cancel or a dismissed prompt do nothing.
            if !matches!(answer.await, Ok(1)) {
                return;
            }
            cx.background_spawn(async { crate::daemon::spawn::stop() })
                .await;
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
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
            let _ = this.update_in(cx, |this, _window, cx| this.restart_daemon_confirmed(cx));
        })
        .detach();
    }

    /// The restart itself, past any confirmation — shared by
    /// [`restart_daemon`](Self::restart_daemon)'s prompt and the startup
    /// version-mismatch prompt
    /// ([`prompt_daemon_version_mismatch`](Self::prompt_daemon_version_mismatch)).
    fn restart_daemon_confirmed(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
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
                        // This window's own workspace only — the other windows
                        // rebuild themselves from theirs.
                        let saved = WorkspaceStore::all(cx)
                            .get(this.workspace)
                            .map(|w| w.session.clone());
                        let pane_ws = this.window_workspace(cx);
                        let (tabs, active) =
                            tabs_from_session(pane_ws.as_ref(), saved, font_size, window, cx);
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
            for leaf in tab.pane.terminals() {
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
            for leaf in tab.pane.terminals() {
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
    /// While the system is being followed, the choice lands in the slot for the
    /// *current* OS appearance (the theme visibly on screen changes either way).
    pub(crate) fn set_preset(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let dark_now = crate::ui::theme::system_dark(cx);
        let cfg = cx.global_mut::<Config>();
        if !cfg.theme_follow_system {
            cfg.theme_preset = id.to_string();
        } else if dark_now {
            cfg.theme_preset_dark = id.to_string();
        } else {
            cfg.theme_preset_light = id.to_string();
        }
        self.after_theme_change(window, cx);
    }

    /// Set the theme for one follow-system slot explicitly (the Light / Dark
    /// cards in Settings). Only visibly changes anything when that slot is the
    /// one currently on screen; either way the choice is persisted.
    pub(crate) fn set_slot_preset(
        &mut self,
        dark_slot: bool,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cfg = cx.global_mut::<Config>();
        if dark_slot {
            cfg.theme_preset_dark = id.to_string();
        } else {
            cfg.theme_preset_light = id.to_string();
        }
        self.after_theme_change(window, cx);
    }

    /// Turn "sync with system appearance" on/off (the Appearance switch).
    /// Turning it off never visibly changes the theme: whatever is on screen
    /// is adopted as the manual choice. Turning it on seeds the slot matching
    /// the manual theme's own brightness with that theme — so the look only
    /// changes when the OS is currently in the *other* mode, where switching
    /// to that mode's slot is exactly what the feature promises.
    pub(crate) fn set_theme_follow_system(
        &mut self,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if on {
            let manual = cx.global::<Config>().theme_preset.clone();
            let manual_dark = crate::ui::presets::by_id(cx, &manual).dark;
            let cfg = cx.global_mut::<Config>();
            cfg.theme_follow_system = true;
            if manual_dark {
                cfg.theme_preset_dark = manual;
            } else {
                cfg.theme_preset_light = manual;
            }
        } else {
            // Resolve while following is still on (the pin is released, so
            // this reads the real OS appearance).
            let effective = crate::ui::theme::effective_preset_id(cx);
            let cfg = cx.global_mut::<Config>();
            cfg.theme_follow_system = false;
            cfg.theme_preset = effective;
        }
        self.after_theme_change(window, cx);
        // Re-aim an open picker panel at a slot that exists in the new mode —
        // after the apply above, so `system_dark` reads the unpinned OS value.
        let slot = if on {
            if crate::ui::theme::system_dark(cx) {
                crate::ui::settings::ThemeSlot::Dark
            } else {
                crate::ui::settings::ThemeSlot::Light
            }
        } else {
            crate::ui::settings::ThemeSlot::Manual
        };
        if let Some(s) = self.active_settings_mut() {
            s.theme_panel_slot = slot;
        }
    }

    /// The shared tail of every theme-selection change: repaint, persist, and
    /// keep the dependent Settings widgets in step.
    fn after_theme_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        apply_theme(Some(window), cx);
        set_menus(cx);
        cx.global::<Config>().save();
        // The editor targets the active theme, so its pickers must track a switch.
        self.rebuild_theme_editor(window, cx);
        // With no global override, the effective opacity follows the theme — keep
        // the Appearance slider's thumb on it.
        self.sync_window_opacity_slider(window, cx);
        cx.notify();
    }

    /// Show/hide the theme picker panel beside the Appearance page. Clicking
    /// the card whose slot the open panel already targets closes it; clicking
    /// another card re-aims the open panel at that slot.
    pub(crate) fn toggle_theme_panel(
        &mut self,
        slot: crate::ui::settings::ThemeSlot,
        cx: &mut Context<Self>,
    ) {
        if let Some(s) = self.active_settings_mut() {
            if s.theme_panel_open && s.theme_panel_slot == slot {
                s.theme_panel_open = false;
            } else {
                s.theme_panel_open = true;
                s.theme_panel_slot = slot;
            }
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
        let id = crate::ui::theme::effective_preset_id(cx);
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

    /// Apply one edit to the active (editable) theme: mutate it, write the
    /// theme's file, reload the registry, and repaint live. The shared tail of
    /// every in-app theme edit (color pickers, opacity slider, blur switch).
    fn mutate_active_theme(
        &mut self,
        mutate: impl FnOnce(&mut crate::ui::presets::Theme),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = crate::ui::theme::effective_preset_id(cx);
        let mut theme = crate::ui::presets::by_id(cx, &id);
        if !theme.editable() {
            return;
        }
        mutate(&mut theme);
        if let Err(e) = crate::ui::presets::write_theme_file(&theme) {
            log::warn!("failed to write theme file: {e}");
            return;
        }
        crate::ui::presets::load_registry(cx);
        apply_theme(Some(window), cx);
        cx.notify();
    }

    /// Apply one color edit to the active (editable) theme.
    pub(crate) fn edit_active_theme(
        &mut self,
        edit: ThemeEdit,
        value: gpui::Hsla,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let c = hsla_to_u32(value);
        self.mutate_active_theme(
            |theme| match edit {
                ThemeEdit::Background => theme.background = Fill::Solid(c),
                ThemeEdit::Foreground => theme.foreground = c,
                ThemeEdit::Accent => theme.accent = c,
                ThemeEdit::Cursor => theme.caret = Some(c),
                ThemeEdit::Selection => theme.selection = Some(c),
                ThemeEdit::Ansi(i) => theme.ansi16[i] = ((c >> 16) as u8, (c >> 8) as u8, c as u8),
            },
            window,
            cx,
        );
    }

    /// The window opacity currently in effect: the global config override when
    /// set, else the active theme's own value, else fully opaque.
    pub(crate) fn effective_window_opacity(cx: &App) -> f32 {
        let config = cx.global::<Config>();
        let theme = crate::ui::presets::by_id(cx, &crate::ui::theme::effective_preset_id(cx));
        config.window_opacity.or(theme.opacity).unwrap_or(1.0)
    }

    /// Set the global window-opacity override from the Appearance slider. Applies
    /// to every theme (persisted in the config, not the theme file).
    pub(crate) fn set_window_opacity(
        &mut self,
        v: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.global_mut::<Config>().window_opacity = Some(v.clamp(0.2, 1.0));
        apply_theme(Some(window), cx);
        cx.global::<Config>().save();
        cx.notify();
    }

    /// Set the global window-blur override from the Appearance switch.
    pub(crate) fn set_window_blur(
        &mut self,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.global_mut::<Config>().window_blur = Some(on);
        apply_theme(Some(window), cx);
        cx.global::<Config>().save();
        cx.notify();
    }

    /// Clear both window overrides so opacity/blur follow the active theme again
    /// (the Appearance section's "Follow theme" action).
    pub(crate) fn reset_window_overrides(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        {
            let config = cx.global_mut::<Config>();
            config.window_opacity = None;
            config.window_blur = None;
        }
        apply_theme(Some(window), cx);
        cx.global::<Config>().save();
        self.sync_window_opacity_slider(window, cx);
        cx.notify();
    }

    /// Snap the Appearance opacity slider's thumb to the value now in effect.
    /// Needed whenever that value changes for a reason other than the user
    /// dragging it (theme switch, "Follow theme" reset).
    pub(crate) fn sync_window_opacity_slider(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let eff = Self::effective_window_opacity(cx);
        if let Some(slider) = self
            .active_settings()
            .map(|s| s.window_opacity_slider.clone())
        {
            slider.update(cx, |s, cx| s.set_value(eff, window, cx));
        }
    }

    /// Set the active (editable) theme's background image from a native file
    /// picker, keeping the existing image opacity (or the schema default).
    pub(crate) fn pick_theme_image(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.mutate_active_theme(
                            |theme| {
                                let opacity =
                                    theme.image.as_ref().map(|i| i.opacity).unwrap_or(0.3);
                                theme.image = Some(crate::ui::presets::Image { path, opacity });
                            },
                            window,
                            cx,
                        );
                        // The editor gains/loses the image-opacity slider with
                        // the image itself.
                        this.rebuild_theme_editor(window, cx);
                    });
                }
            }
        })
        .detach();
    }

    /// Remove the active theme's background image.
    pub(crate) fn remove_theme_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mutate_active_theme(|theme| theme.image = None, window, cx);
        self.rebuild_theme_editor(window, cx);
    }

    /// Set the active theme's background-image opacity from the editor slider.
    pub(crate) fn set_theme_image_opacity(
        &mut self,
        v: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mutate_active_theme(
            |theme| {
                if let Some(img) = theme.image.as_mut() {
                    img.opacity = v.clamp(0.0, 1.0);
                }
            },
            window,
            cx,
        );
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
        let id = crate::ui::theme::effective_preset_id(cx);
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

        // Background-image opacity slider, present only while the theme has an
        // image (choosing/removing one rebuilds the editor). Emits `Change`
        // continuously while dragging; each tick writes the theme file and
        // repaints, so the mix is live under the thumb.
        let image_opacity_slider = theme.image.as_ref().map(|img| {
            let slider = cx.new(|_| {
                SliderState::new()
                    .min(0.0)
                    .max(1.0)
                    .step(0.01)
                    .default_value(img.opacity)
            });
            subs.push(cx.subscribe_in(
                &slider,
                window,
                |this, _s, ev: &SliderEvent, window, cx| {
                    if let SliderEvent::Change(v) = ev {
                        this.set_theme_image_opacity(v.start(), window, cx);
                    }
                },
            ));
            slider
        });

        if let Some(s) = self.settings.as_mut() {
            s.theme_editor = Some(ThemeEditor {
                for_id: theme.id.clone(),
                seed,
                ansi,
                image_opacity_slider,
                _subs: subs,
            });
        }
    }

    /// Toggle terminal font ligatures through the generic `font_features`
    /// config. On enables the common programming-font features; off restores
    /// tty7's terminal-safe default (contextual ligatures disabled).
    pub(crate) fn set_font_ligatures(&mut self, on: bool, cx: &mut Context<Self>) {
        let features = on.then(|| {
            crate::core::config::FontFeatures(Arc::new(vec![
                ("calt".to_string(), 1),
                ("liga".to_string(), 1),
            ]))
        });
        // The config holds the gpui-free representation; the views want gpui's.
        let gpui_features = features
            .as_ref()
            .map(crate::core::config::gpui_font_features);
        self.font_features = gpui_features.clone();
        for tab in &self.tabs {
            for leaf in tab.pane.terminals() {
                let features = gpui_features.clone();
                leaf.update(cx, |v, cx| v.set_font_features(features, cx));
            }
        }
        let cfg = cx.global_mut::<Config>();
        cfg.font_features = features;
        cfg.save();
        cx.notify();
    }

    fn apply_terminal_config_to_panes(&self, config: &Config, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            for leaf in tab.pane.terminals() {
                leaf.update(cx, |v, cx| {
                    v.terminal.apply_user_config(config);
                    cx.notify();
                });
            }
        }
    }

    /// Switch the default cursor shape, update each pane's terminal defaults,
    /// and repaint. App-requested DECSCUSR shapes still override this at runtime.
    pub(crate) fn set_cursor_style(&mut self, style: ConfigCursorStyle, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.cursor_style = style);
        let cfg = cx.global::<Config>().clone();
        self.terminal_cursor_style = cfg.cursor_style;
        self.terminal_scrollback_limit = cfg.scrollback_limit;
        self.apply_terminal_config_to_panes(&cfg, cx);
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

    /// How a forward on `pane_id` reaches the daemon (design §15).
    ///
    /// Looked up across every tab's leaves rather than off the focused one: the
    /// Forwards band tracks the pane the *panel* is showing, which is not
    /// necessarily the pane with keyboard focus.
    pub(crate) fn forward_route(&self, pane_id: u64, cx: &gpui::App) -> ForwardRoute {
        let workspace = self
            .tabs
            .iter()
            .flat_map(|tab| tab.pane.terminals())
            .find_map(|leaf| {
                let view = leaf.read(cx);
                (view.pane_id == pane_id).then(|| view.workspace().cloned())?
            });
        ForwardRoute { pane_id, workspace }
    }

    /// Refresh the managed (Local/Remote/Dynamic) forwards for `pane_id` (WS4).
    pub(crate) fn refresh_managed_forwards(&mut self, pane_id: u64, cx: &mut Context<Self>) {
        self.loopback_panel.managed = self.forward_route(pane_id, cx).list();
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
        let route = self.forward_route(pane_id, cx);
        if let Some(old_id) = self.loopback_panel.mf_editing.take() {
            let _ = route.remove(old_id);
        }
        self.loopback_panel.managed = route.add(rule);
        // The new row *is* the confirmation, so the form folds away rather than
        // sitting there re-inviting an add nobody asked for. (Only on the success
        // path — every validation failure above returns early with it still open.)
        self.loopback_panel.form_pane_id = None;
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
        // Clicking a row is the only way in, and the form is where the values
        // land — so expand it on the row's own pane.
        self.loopback_panel.form_pane_id = Some(forward.pane_id);
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
        self.loopback_panel.managed = self.forward_route(pane_id, cx).remove(forward_id);
        cx.notify();
    }

    /// `ShowSshForwards` / the palette's "SSH: Port Forwarding": land on the
    /// pane's forwards wherever you were. The band lives on the Info tab, so this
    /// opens the panel there and expands the add form — the one entry point that
    /// works with the panel closed, which is why it exists at all.
    ///
    /// A no-op on anything but a connected native-SSH pane: without a connection
    /// there is nothing to forward over, and opening an empty form on a local
    /// shell would only be a puzzle.
    pub(crate) fn show_ssh_forwards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pane_id, _)) = self.active_connected_native_ssh_pane(window, cx) else {
            return;
        };
        self.set_right_panel_tab(crate::core::config::RightPanelTab::Info, cx);
        if self.loopback_panel.form_pane_id != Some(pane_id) {
            self.toggle_managed_forward_form(pane_id, window, cx);
        }
    }

    /// The Forwards band's `+`: expand the add form for `pane_id`, or collapse it
    /// if it's already this pane's. Collapsing goes through the same reset as
    /// Cancel, so a form abandoned mid-edit can't come back still in edit mode.
    pub(crate) fn toggle_managed_forward_form(
        &mut self,
        pane_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.loopback_panel.form_pane_id == Some(pane_id) {
            self.close_managed_forward_form(window, cx);
            return;
        }
        self.loopback_panel.form_pane_id = Some(pane_id);
        self.cancel_managed_forward_edit(window, cx);
        self.refresh_managed_forwards(pane_id, cx);
    }

    /// Collapse the add/edit form, clearing it back to the add defaults.
    pub(crate) fn close_managed_forward_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loopback_panel.form_pane_id = None;
        self.cancel_managed_forward_edit(window, cx);
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

    /// Toggle inactive-pane dimming. Applies on the next render — `update_config`
    /// notifies, and this view's render is what hands the flag to the pane tree.
    pub(crate) fn set_dim_inactive_panes(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.dim_inactive_panes = on);
    }

    pub(crate) fn set_cursor_blink(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.cursor_blink = on);
        // Turning blink off mid-cycle could leave the cursor in its hidden phase;
        // force every pane's cursor back on so it doesn't stick invisible.
        if !on {
            for tab in &self.tabs {
                for leaf in tab.pane.terminals() {
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
        let cfg = cx.global::<Config>().clone();
        self.terminal_cursor_style = cfg.cursor_style;
        self.terminal_scrollback_limit = cfg.scrollback_limit;
        self.apply_terminal_config_to_panes(&cfg, cx);
    }

    pub(crate) fn set_new_tab_position(&mut self, pos: NewTabPosition, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.new_tab_position = pos);
    }

    /// Set where the tab bar is rendered (Settings → Window & Tabs). Persists the
    /// choice; the layout re-derives from the `Config` global on the next render.
    pub(crate) fn set_tab_bar_position(&mut self, pos: TabBarPosition, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.tab_bar_position = pos);
    }

    /// Set how the vertical tab sidebar arranges its rows (Settings → Window &
    /// Tabs): grouped per git repo or one flat list. Persists the choice; the
    /// sidebar re-derives from the `Config` global on the next render.
    pub(crate) fn set_sidebar_grouping(
        &mut self,
        grouping: crate::core::config::SidebarGrouping,
        cx: &mut Context<Self>,
    ) {
        self.update_config(cx, |cfg| cfg.sidebar_grouping = grouping);
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

    /// `ToggleLeftPanel` (⌘B): collapse/expand the left rail in place, persisting
    /// the choice. In `Top` mode there is no rail to collapse, so this switches to
    /// `Left` and shows it — the shortcut always means "give me the sidebar".
    pub(crate) fn toggle_left_panel(&mut self, cx: &mut Context<Self>) {
        let (pos, collapsed) = match cx.global::<Config>().tab_bar_position {
            TabBarPosition::Top => (TabBarPosition::Left, false),
            // This window's own collapse state — collapsing one window's rail
            // must not collapse every other window's. See `sidebar_collapsed`.
            TabBarPosition::Left => (TabBarPosition::Left, !self.sidebar_collapsed),
        };
        self.sidebar_collapsed = collapsed;
        self.update_config(cx, |cfg| {
            cfg.tab_bar_position = pos;
            cfg.sidebar_collapsed = collapsed;
        });
        cx.notify();
    }

    /// Whether the left rail is actually on screen: `Left` mode, not collapsed,
    /// and at least one tab (the home page has no rail). The layout, the title
    /// strip and the collapse button all derive from this one predicate.
    pub(crate) fn left_panel_open(&self, cx: &gpui::App) -> bool {
        matches!(cx.global::<Config>().tab_bar_position, TabBarPosition::Left)
            && !self.sidebar_collapsed
            && !self.tabs.is_empty()
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

    /// Toggle the system tray icon. The tray's poll loop re-reads the flag
    /// every second, so the icon appears/disappears without a restart.
    pub(crate) fn set_show_tray_icon(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.show_tray_icon = on);
    }

    /// Toggle the "Close Window?" prompt on the last window. The close handler
    /// reads the flag when it fires, so this applies to the very next ⌘W with no
    /// restart and nothing to push to open windows.
    pub(crate) fn set_confirm_window_close(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.confirm_window_close = on);
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
            for leaf in tab.pane.terminals() {
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

    pub(crate) fn set_smart_select(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.smart_select = on);
    }

    pub(crate) fn set_tab_completion(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.tab_completion = on);
    }

    pub(crate) fn set_history_search(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.history_search = on);
    }

    pub(crate) fn set_startup_mode(
        &mut self,
        mode: crate::core::config::StartupMode,
        cx: &mut Context<Self>,
    ) {
        self.update_config(cx, |cfg| cfg.startup_mode = mode);
    }

    pub(crate) fn set_remember_window_size(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.remember_window_size = on);
    }

    /// Name the OS window after its workspace, so ⌘` and Mission Control can
    /// tell several tty7 windows apart. Reads the *saved* workspace, which
    /// every structural change writes just before focus lands here — a title
    /// one beat behind is invisible, and it keeps this off the render path.
    ///
    /// An empty workspace has no subject yet, so it falls back to the app name
    /// rather than showing "Untitled".
    pub(crate) fn sync_window_title(&self, window: &mut Window, cx: &App) {
        let title = WorkspaceStore::all(cx)
            .get(self.workspace)
            .filter(|w| !w.session.tabs.is_empty())
            .map(|w| w.display_name())
            .unwrap_or_else(|| "tty7".to_string());
        if *self.window_title.borrow() == title {
            return;
        }
        window.set_window_title(&title);
        *self.window_title.borrow_mut() = title;
    }

    pub(crate) fn focus_active(&self, window: &mut Window, cx: &mut App) {
        // Focus moves after every structural change, which is exactly when the
        // window's subject may have changed too.
        self.sync_window_title(window, cx);
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
        // A tab showing its diff overlay gives the overlay focus (Esc-to-close
        // must keep working when switching back to it); `close_diff_overlay`
        // re-runs this after clearing the slot to land on the terminal.
        if let Some(overlay) = tab.diff_overlay.as_ref() {
            window.focus(&overlay.focus_handle, cx);
            return;
        }
        if let Some(leaf) = tab.focus_target() {
            let handle = leaf.focus_handle(cx);
            window.focus(&handle, cx);
        }
    }

    /// Snapshot which pane currently holds focus in the active tab into that
    /// tab's `last_focused`, so `focus_active` can restore it when we come back.
    /// Call this before any transition that moves focus off the active tab
    /// (switching tabs, opening a focus-stealing overlay).
    pub(crate) fn remember_active_pane(&mut self, window: &Window, cx: &App) {
        let active = self.active;
        if let Some(tab) = self.tabs.get_mut(active) {
            if let Some(leaf) = tab.pane.focused_leaf(window, cx) {
                tab.last_focused = Some(leaf.entity_id());
            }
        }
    }

    fn focus_leaf(&self, leaf: &PaneSlot, window: &mut Window, cx: &mut App) {
        let handle = leaf.focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Put a finished (or failed) remote pane back into the tree.
    ///
    /// `slot_id` is the *placeholder's* id, which is what the tree still holds:
    /// the terminal that just arrived has an identity of its own and has never
    /// been in the tree.
    fn land_pane(
        &mut self,
        slot_id: gpui::EntityId,
        pending: &Entity<crate::ui::pending_pane::PendingPane>,
        parts: Result<crate::terminal::view::ShellParts, String>,
        font_size: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let parts = match parts {
            Ok(parts) => parts,
            // §17: the slot keeps its place and says what went wrong. It does
            // not collapse the split under the user, and it does not close the
            // tab — both would throw away a layout because a network blinked.
            Err(reason) => {
                pending.update(cx, |p, cx| p.fail(reason, cx));
                return;
            }
        };
        // The pane or its tab may have been closed while the connect was in the
        // air. Checked *before* the view is built, because the answer decides
        // whether to build one at all — and because the pane on the other
        // machine is real and running either way, so a slot that is gone means
        // killing it rather than leaking it. Nothing on this client can reach
        // it any more, and closing the slot is the user saying they do not want
        // it (unlike a quit, where panes are deliberately detached and kept).
        let still_there = self
            .tabs
            .iter()
            .any(|tab| tab.pane.leaves().iter().any(|l| l.entity_id() == slot_id));
        if !still_there {
            log::info!(
                "pane {} arrived after its slot closed; killing it",
                parts.pane_id
            );
            let route = crate::terminal::PaneRoute::for_workspace(parts.workspace.as_ref());
            kill_pane_off_thread(route, parts.pane_id, cx);
            return;
        }
        // Whether the user was sitting on this pane while it connected. Read
        // before the swap, since the placeholder leaves the tree in it.
        let was_focused = pending.read(cx).focus_handle.contains_focused(window, cx);
        let view = build_terminal_view(parts, font_size, window, cx);
        let slot = PaneSlot::Ready(view.clone());
        self.tabs
            .iter_mut()
            .any(|tab| tab.pane.replace_leaf(slot_id, slot.clone()));
        if was_focused {
            self.focus_leaf(&slot, window, cx);
        }
        // The pane has a real `pane_id` now, which is what restore matches on.
        self.save_session(cx);
        cx.notify();
    }

    /// Ask every pane in the window to re-probe its git status. Called when the
    /// window regains focus: the sidebar shows a git line for *every* tab, not
    /// just the active one, so refreshing only the focused pane would leave the
    /// rest of the list stale — which is exactly the list the user is scanning
    /// right after switching back.
    ///
    /// Panes sharing a cwd fold into one probe in the shared cache, and the
    /// throttle there counts per repo rather than per cwd, so once the cache
    /// knows where each pane lives the cost of a window with many panes is
    /// bounded by the number of distinct repos — not by the number of
    /// subdirectories they happen to sit in, which would be the same full-repo
    /// `git diff` asked several times over.
    fn refresh_git_status_all(&mut self, cx: &mut Context<Self>) {
        for leaf in self.tabs.iter().flat_map(|tab| tab.pane.terminals()) {
            leaf.update(cx, |view, cx| view.refresh_git_status_now(cx));
        }
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
        // A window is one machine (design §2). A remote workspace's window must
        // not open a shell on *this* computer, so the refusal happens before
        // anything is spawned rather than after a local pane is already in the
        // tab strip.
        if !self.guard_local_spawn(window, cx) {
            return;
        }
        // Inherit the cwd of the active tab's focused terminal so the new tab
        // opens in the same directory the user is currently working in. The new
        // tab takes this window's route, so it lands on the same machine the
        // source pane's shell is on — which is what `spawnable_cwd` gates on. A
        // pane whose shell is somewhere else entirely (a native-SSH or WSL pane
        // in an otherwise local window) declines, because an inherited cwd wins
        // over every fallback in `pane::initial_working_directory` and would go
        // straight to the spawn as a working directory.
        let cwd = self.tabs.get(self.active).and_then(|t| {
            t.pane
                .focused_or_first(window, cx)
                .and_then(|leaf| leaf.read(cx).spawnable_cwd())
        });
        let pane_ws = self.window_workspace(cx);
        let tab = match new_terminal(pane_ws, self.font_size, cwd, None, shell, window, cx) {
            Ok(view) => view,
            Err(e) => {
                log::error!("new tab spawn failed: {e}");
                window.push_notification(format!("Could not open a terminal: {e}"), cx);
                return;
            }
        };
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
        self.tabs
            .insert(insert_at, Tab::new(Pane::leaf(PaneSlot::Ready(view))));
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
            if tab
                .pane
                .replace_leaf(dead.entity_id(), PaneSlot::Ready(fresh.clone()))
            {
                break;
            }
        }
        self.maximized = None;
        self.focus_leaf(&PaneSlot::Ready(fresh), window, cx);
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
        // more WSL/fish, not back to the default). Same rule as a new tab: the
        // split takes this window's route, so `spawnable_cwd` is the cwd the
        // machine it lands on can actually chdir into. (The native-SSH branch
        // below has the daemon discard it regardless.)
        if !self.guard_local_spawn(window, cx) {
            return;
        }
        let cwd = target.read(cx).spawnable_cwd();
        // Splitting a native-SSH pane opens another SSH pane on the same
        // connection rather than dropping back to a local shell. Re-resolve the
        // persisted (secret-free) spec from its saved profile so keychain
        // secrets are re-applied, mirroring the reconnect path.
        let ssh_spec = target.read(cx).ssh_spec();
        let new = if let Some(spec) = ssh_spec {
            let resolved = crate::ui::ssh_connect::resolve_persisted_ssh_spec(spec, cx);
            match new_terminal_native(self.font_size, cwd, resolved, window, cx) {
                Ok(view) => PaneSlot::Ready(view),
                Err(e) => {
                    log::error!("native SSH split spawn failed: {e}");
                    window.push_notification(format!("SSH connection failed: {e}"), cx);
                    return;
                }
            }
        } else {
            let shell = target.read(cx).shell_spec();
            match new_terminal(
                self.window_workspace(cx),
                self.font_size,
                cwd,
                None,
                shell,
                window,
                cx,
            ) {
                Ok(view) => view,
                Err(e) => {
                    log::error!("split spawn failed: {e}");
                    window.push_notification(format!("Could not split the pane: {e}"), cx);
                    return;
                }
            }
        };
        if let Some(tab) = self.tabs.get_mut(self.active) {
            if tab
                .pane
                .split_leaf(target.entity_id(), axis, false, new.clone())
            {
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
                .find(|l| l.contains_focused(window, cx))
                // A pane still connecting has no daemon pane to kill; the
                // spawn that is in the air for it is handled where it lands
                // (`land_pane` finds no slot and kills it there).
                .and_then(|l| l.terminal().cloned())
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
                    kill_pane_off_thread(leaf.read(cx).pane_route(), leaf.read(cx).pane_id, cx);
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
        match self.tabs[index].pane.close_leaf(view.entity_id()) {
            // The exited pane was the tab's only leaf: close the whole tab
            // (which snapshots it for reopen and kills its daemon panes).
            CloseOutcome::RemoveSelf => self.close_tab(index, window, cx),
            // Unreachable — containment was just checked — but never close a
            // tab we failed to locate the leaf in.
            CloseOutcome::NotFound => {}
            CloseOutcome::Collapsed => {
                kill_pane_off_thread(view.read(cx).pane_route(), view.read(cx).pane_id, cx);
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
            .position(|l| l.contains_focused(window, cx))
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
            // The incoming tab may have a diff overlay that went stale while
            // hidden (its repo changed underneath); re-probe if the status
            // cache disagrees with the shown snapshot.
            self.maybe_refresh_diff_overlay(cx);
            // In sidebar mode, pull the newly active row into view (a no-op when
            // the strip is horizontal — the handle tracks no painted list then).
            self.sidebar_scroll.scroll_to_item(index);
            if self.code_panel_visible() {
                // The incoming tab has its own panel open: refresh its roots
                // (pane cwds may have changed) and keep focus on the panel.
                self.file_tree_refresh_roots(window, cx);
                self.file_tree.focus_handle.focus(window, cx);
            } else {
                self.focus_active(window, cx);
            }
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
        // Capture the tab's cwd *before* its panes are killed (the daemon can't
        // report it afterwards): if it sat in a tty7-managed worktree, the
        // cleanup offer below needs it.
        let worktree_cwd = self.tab_host_cwd(index, window, cx);
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
        for leaf in self.tabs[index].pane.terminals() {
            kill_pane_off_thread(leaf.read(cx).pane_route(), leaf.read(cx).pane_id, cx);
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
        // The tab is gone; if it lived in a tty7-managed worktree, offer to
        // clean the checkout up rather than letting them pile up silently.
        self.offer_worktree_cleanup(worktree_cwd, cx);
    }

    /// After closing a tab that sat in a tty7-managed worktree (see
    /// [`crate::core::worktree::managed`]), offer to remove the checkout: a
    /// clean worktree gets a plain keep/remove prompt; one with uncommitted
    /// changes defaults to keeping and makes discarding explicit. Removal also
    /// deletes the branch when it carries no unmerged commits (`branch -d`).
    /// No offer while any surviving pane still has its cwd inside the checkout
    /// (new tabs inherit the current cwd, so shared worktrees are common) —
    /// removal would yank the directory out from under a live shell.
    /// Detection, the dirty probe, and removal all run off the UI thread.
    fn offer_worktree_cleanup(
        &mut self,
        cwd: Option<(crate::ui::host_ops::SharedHost, std::path::PathBuf)>,
        cx: &mut Context<Self>,
    ) {
        let Some((host, cwd)) = cwd else { return };
        // Every leaf of every surviving tab, not just focused panes — a shell
        // tucked away in a split occupies the worktree all the same. Only panes
        // on the *same* host count: this list is what stops a worktree being
        // removed out from under a live shell, and a cwd on another machine can
        // neither occupy this worktree nor be compared against it meaningfully.
        let id = host.id();
        let open_cwds: Vec<std::path::PathBuf> = self
            .tabs
            .iter()
            .flat_map(|tab| tab.pane.terminals())
            .filter_map(|leaf| {
                let view = leaf.read(cx);
                (view.host_id() == id).then(|| view.host_cwd())?
            })
            .collect();
        // Two host round trips with a *user decision* between them, so it is two
        // `HostOps` calls rather than one long background block: resolve what
        // was closed, ask, then remove. Nothing here touches the host from the
        // UI thread, and the prompt is awaited between the two.
        let remove_host = host.clone();
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| {
                crate::core::worktree::managed(h, &cwd)
                    .filter(|wt| !crate::core::worktree::occupied(h, &wt.path, &open_cwds))
            },
            move |_this, found, cx| {
                let Some(wt) = found else { return };
                let detail = if wt.dirty {
                    format!(
                        "The closed tab's worktree at {} has uncommitted changes.",
                        wt.path.display()
                    )
                } else {
                    format!(
                        "The closed tab's worktree at {} is clean.",
                        wt.path.display()
                    )
                };
                let title = format!("Remove worktree \"{}\"?", wt.branch);
                let level = if wt.dirty {
                    PromptLevel::Warning
                } else {
                    PromptLevel::Info
                };
                let remove_label = if wt.dirty {
                    "Discard Changes & Remove"
                } else {
                    "Remove Worktree"
                };
                cx.spawn(async move |this, cx| {
                    let Ok(answer) = this.update_in(cx, |_, window, cx| {
                        window.prompt(level, &title, Some(&detail), &["Keep", remove_label], cx)
                    }) else {
                        return;
                    };
                    if !matches!(answer.await, Ok(1)) {
                        return;
                    }
                    let force = wt.dirty;
                    let branch = wt.branch.clone();
                    let _ = this.update_in(cx, |_, window, cx| {
                        crate::ui::host_ops::HostOps::run_in(
                            remove_host,
                            window,
                            cx,
                            move |h| crate::core::worktree::remove(h, &wt, force),
                            move |_this, result, window, cx| match result {
                                Ok(()) => window.push_notification(
                                    format!("Removed worktree \"{branch}\""),
                                    cx,
                                ),
                                Err(e) => window
                                    .push_notification(format!("Worktree removal failed: {e}"), cx),
                            },
                        );
                    });
                })
                .detach();
            },
        );
    }

    /// Close every tab except `index` ("Close Other Tabs"). Iterates from the
    /// end so removals never shift an index still to visit. Tabs holding a live
    /// warn-on-close SSH session are skipped outright — the per-tab confirm
    /// sheet is keyed by index, which a bulk close would immediately
    /// invalidate — so they simply survive the sweep.
    pub(crate) fn close_other_tabs(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.tabs.len() {
            return;
        }
        for i in (0..self.tabs.len()).rev() {
            if i == index || self.tab_has_warn_ssh(i, cx) {
                continue;
            }
            self.close_tab(i, window, cx);
        }
    }

    /// Close every tab after `index` ("Close Tabs to the Right" / "Close Tabs
    /// Below" in the sidebar). Same end-first iteration and warn-SSH skip as
    /// [`close_other_tabs`](Self::close_other_tabs).
    pub(crate) fn close_tabs_right_of(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for i in ((index + 1)..self.tabs.len()).rev() {
            if self.tab_has_warn_ssh(i, cx) {
                continue;
            }
            self.close_tab(i, window, cx);
        }
    }

    /// "Mark as Unread" (tab context menu): re-flag every finished (`Done`)
    /// agent turn in the tab as unread, so the avatar's green dot swells back
    /// into its count badge until the user next looks at those panes. The
    /// active tab's focus target is told the dismissed menu is about to hand
    /// focus back to it, so that focus-in doesn't immediately re-read the mark
    /// (see `TerminalView::mark_agent_result_unread`).
    pub(crate) fn mark_tab_unread(&mut self, index: usize, cx: &mut Context<Self>) {
        use crate::core::cli_agent::AgentStatus;
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let refocus = (index == self.active).then(|| tab.focus_target()).flatten();
        for leaf in tab.pane.terminals() {
            let refocus_incoming =
                refocus.as_ref().map(|s| s.entity_id()) == Some(leaf.entity_id());
            leaf.update(cx, |view, cx| {
                if view.agent_session().map(|s| s.status) == Some(AgentStatus::Done) {
                    view.mark_agent_result_unread(refocus_incoming);
                    cx.notify();
                }
            });
        }
        cx.notify();
    }

    /// The cwd of the tab's label-driving terminal (focused leaf, else first) —
    /// what the tab context menu's "Copy Working Directory" copies and "New
    /// Worktree Tab" derives the repo from.
    pub(crate) fn tab_cwd(
        &self,
        index: usize,
        window: &Window,
        cx: &App,
    ) -> Option<std::path::PathBuf> {
        self.tabs
            .get(index)?
            .pane
            .focused_or_first(window, cx)
            .and_then(|leaf| leaf.read(cx).cwd())
    }

    /// Copy the active tab's working directory to the clipboard — the
    /// `CopyWorkingDirectory` action behind the File menu, the palette, and the
    /// tab context menu's row of the same name. A no-op when the pane has yet to
    /// report a cwd, which is also when the context-menu row renders disabled.
    pub(crate) fn copy_active_cwd(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(cwd) = self.tab_cwd(self.active, window, cx) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(cwd.display().to_string()));
        }
    }

    /// What the tab's agent-session menu rows ("Fork Session" and "Copy Session
    /// ID") need, together with the pane they were read from, or `None` when
    /// the tab's label-driving pane runs no coding agent — then neither row is
    /// offered. Reads the same leaf `tab_cwd` does, so all three rows agree on
    /// which pane a tab-level action means. The pane comes back with the state
    /// because the fork row has to *act* on it: by click time the popup menu
    /// holds focus and sits outside every terminal's focus path, so resolving
    /// the pane again there would fall back to the tab's first leaf.
    pub(crate) fn tab_agent_session(
        &self,
        index: usize,
        window: &Window,
        cx: &App,
    ) -> Option<(Entity<TerminalView>, TabAgentSession)> {
        let leaf = self.tabs.get(index)?.pane.focused_or_first(window, cx)?;
        let view = leaf.read(cx);
        let agent = view.agent()?;
        let session = TabAgentSession {
            fork_label: agent.fork_label(),
            session_id: view.agent_session().and_then(|s| s.session_id),
            remote: view.remote_context().is_some(),
        };
        Some((leaf, session))
    }

    /// "Copy Session ID": put the agent's native session id on the clipboard —
    /// the id `codex resume` / `claude --resume` take. A no-op when no agent
    /// has reported one, which is also when the menu row renders disabled.
    pub(crate) fn copy_agent_session_id(
        &mut self,
        index: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self
            .tab_agent_session(index, window, cx)
            .and_then(|(_, s)| s.session_id)
        {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(id));
        }
    }

    /// "Fork Session" (issue #211): branch the agent session running in
    /// `source` — a leaf of tab `index` — into a second, independent one,
    /// landing it per `placement`.
    ///
    /// The source pane is the caller's to resolve, because *when* it is
    /// resolved differs by surface. The action paths (palette, menu bar,
    /// keybinding, pane right-click menu) dispatch through the terminal's own
    /// `action_context`, so the focused leaf read at dispatch time is the pane
    /// the user is pointing at — [`fork_active_pane_session`](Self::fork_active_pane_session)
    /// does that for them. The tab / sidebar context menu can't: by the time
    /// its row is clicked the popup holds focus, so it captures the pane its
    /// row was labelled for at menu-open time and hands it in here.
    ///
    /// The fork itself is entirely the agent's own — tty7 spawns a pane and
    /// types the agent's fork command into it (`codex fork <id>`, `claude
    /// --resume <id> --fork-session`, …), exactly as session restore types a
    /// resume command. tty7 never reads or writes the agent's transcript files,
    /// so a change to their on-disk format costs at most a visible shell error
    /// in the new pane.
    ///
    /// Every reason a fork can't happen surfaces as a notification rather than
    /// a silent no-op: the menu rows disable themselves for the same reasons,
    /// but the action is also reachable from the palette, the menu bar and a
    /// bound key, where there is no row to grey out.
    pub(crate) fn fork_agent_session(
        &mut self,
        index: usize,
        source: Entity<TerminalView>,
        placement: ForkPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cmd) = self.agent_fork_command(&source, window, cx) else {
            return;
        };

        // A split acts on the *active* tab's focused pane, so bring the
        // right-clicked tab forward first — a no-op when it already is, and the
        // same order the context menu's own Split rows use. Done before the
        // terminal is created, since constructing one steals focus.
        if matches!(placement, ForkPlacement::Split { .. }) {
            self.activate(index, window, cx);
        }

        // The fork inherits the source pane's directory and shell pick, like
        // every other tty7 spawn. Deliberately *not* passed to the agent as a
        // `--cd`: Codex has its own resume/fork cwd preference and this must
        // not override the setting the user chose there.
        let (cwd, shell) = {
            let view = source.read(cx);
            (view.local_cwd(), view.shell_spec())
        };
        let new = match new_terminal(
            self.window_workspace(cx),
            self.font_size,
            cwd,
            None,
            shell,
            window,
            cx,
        ) {
            Ok(view) => view,
            Err(e) => {
                log::error!("fork spawn failed: {e}");
                window.push_notification(format!("Could not open a terminal: {e}"), cx);
                return;
            }
        };
        // Same hand-off session restore uses: the bytes queue in the PTY until
        // the (still starting) shell reads them.
        //
        // A slot that is still connecting has no terminal to hand the command
        // to. Forking gates on a *local* pane, so this is unreachable in
        // practice — but say so rather than placing a pane that silently never
        // forks.
        let Some(terminal) = new.terminal() else {
            log::error!("fork spawn produced a pane that is still connecting");
            window.push_notification("Could not fork: the pane is still connecting", cx);
            return;
        };
        terminal.read(cx).run_command_line(&cmd);

        match placement {
            ForkPlacement::NewTab => {
                self.remember_active_pane(window, cx);
                self.maximized = None;
                let insert_at = self.new_tab_insert_at(cx);
                self.tabs.insert(insert_at, Tab::new(Pane::leaf(new)));
                self.active = insert_at;
                self.focus_active(window, cx);
            }
            ForkPlacement::Split { axis, before } => {
                let placed = self.tabs.get_mut(index).is_some_and(|tab| {
                    tab.pane
                        .split_leaf(source.entity_id(), axis, before, new.clone())
                });
                if !placed {
                    return;
                }
                self.maximized = None;
                self.focus_leaf(&new, window, cx);
            }
        }
        self.save_session(cx);
        cx.notify();
    }

    /// Fork the active tab's focused pane (else its first) per `placement` —
    /// every surface that dispatches an action rather than clicking a captured
    /// menu row. Those all route through the terminal's `action_context`, so
    /// the focused leaf resolved here is the pane the user is pointing at.
    /// Silently a no-op for a tab with no terminal; every other reason a fork
    /// can't run is a notification from
    /// [`agent_fork_command`](Self::agent_fork_command).
    pub(crate) fn fork_active_pane_session(
        &mut self,
        placement: ForkPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
        else {
            return;
        };
        self.fork_agent_session(self.active, source, placement, window, cx);
    }

    /// Fork the active tab's focused pane into a split beside it — the pane
    /// right-click menu's placement pick, and what a bound key means (the
    /// focused pane is the one the user is pointing at).
    pub(crate) fn fork_focused_pane_session(
        &mut self,
        axis: Axis,
        before: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fork_active_pane_session(ForkPlacement::Split { axis, before }, window, cx);
    }

    /// The fork command to type into a new pane for `source`'s agent session,
    /// or `None` after telling the user why there isn't one.
    fn agent_fork_command(
        &self,
        source: &Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        use crate::core::cli_agent::AgentStatus;
        let view = source.read(cx);
        let (agent, session, remote) = (view.agent(), view.agent_session(), view.remote_context());
        let Some(agent) = agent else {
            window.push_notification("This pane isn't running a coding agent", cx);
            return None;
        };
        let name = agent.display_name();
        if agent.fork_label().is_none() {
            window.push_notification(format!("tty7 has no fork command for {name}"), cx);
            return None;
        }
        if remote.is_some() {
            window.push_notification(
                format!("{name} sessions can only be forked from a local pane"),
                cx,
            );
            return None;
        }
        let session = session.unwrap_or_default();
        let Some(id) = session.session_id.as_deref() else {
            window.push_notification(
                format!("tty7 hasn't seen a {name} session id in this pane — install its hooks in Settings → Agents"),
                cx,
            );
            return None;
        };
        let Some(cmd) = agent.fork_command(id, session.launch_argv.as_deref()) else {
            window.push_notification(format!("{name}'s session id isn't a plain token"), cx);
            return None;
        };
        // Agents fork from the *persisted* transcript, so a turn still in
        // flight is simply absent from the fork (Codex documents that an
        // in-progress turn cannot even be a fork point). Harmless — the parent
        // is untouched — but the user must not have to discover it.
        if session.status == AgentStatus::Working {
            window.push_notification(
                format!("{name} is mid-turn — the fork won't include the turn in flight"),
                cx,
            );
        }
        Some(cmd)
    }

    /// An explicit "check now", from the App menu or the tray. Forced, so it
    /// works even with the startup check turned off — "I asked" outranks "don't
    /// ask on my behalf" — and it opens About, where the result lands.
    pub(crate) fn check_for_updates_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        crate::core::update::spawn_check_forced(cx);
        self.open_settings_section(SettingsSection::About, window, cx);
    }

    /// [`tab_cwd`](Self::tab_cwd) paired with the host that can answer for it —
    /// for the worktree operations, which run `git` on the machine the
    /// repository is actually on. `None` when the tab's pane has no cwd its own
    /// host could be asked about (see
    /// [`TerminalView::host_cwd`](crate::terminal::view::TerminalView::host_cwd)).
    ///
    /// "Copy Working Directory" deliberately keeps using `tab_cwd`: copying a
    /// remote pane's remote path is exactly what the user wants there.
    fn tab_host_cwd(
        &self,
        index: usize,
        window: &Window,
        cx: &App,
    ) -> Option<(crate::ui::host_ops::SharedHost, std::path::PathBuf)> {
        let leaf = self.tabs.get(index)?.pane.focused_or_first(window, cx)?;
        let view = leaf.read(cx);
        Some((view.host(cx)?, view.host_cwd()?))
    }

    /// Whether tab `index` sits inside a git repository — what gates the
    /// context menu's "New Worktree Tab" entry.
    ///
    /// Read from the shared [`GitStatusCache`](crate::terminal::git_status::GitStatusCache)
    /// rather than probed: menus are built synchronously on the UI thread, and
    /// asking a host is a blocking call that on a remote machine is a round
    /// trip. The cache already holds this answer — the pane's git line is
    /// derived from the same probe — so the entry appears exactly when the
    /// branch line does. `Some(None)` is a probe that answered "not a repo";
    /// `None` is "no probe has landed yet", which reads as no entry rather than
    /// as an entry that would immediately fail.
    pub(crate) fn tab_is_in_repo(&self, index: usize, window: &Window, cx: &App) -> bool {
        let Some(leaf) = self
            .tabs
            .get(index)
            .and_then(|t| t.pane.focused_or_first(window, cx))
        else {
            return false;
        };
        let view = leaf.read(cx);
        // `git_status_cwd`, not the live `host_cwd`: the cache is *keyed* by
        // the cwd the last probe was launched for, so asking it about the
        // pane's current foreground cwd would miss for the whole window
        // between a `cd` and the next probe landing — and the entry would
        // vanish from the menu exactly when the user just navigated into a
        // repository.
        let Some(cwd) = view.git_status_cwd() else {
            return false;
        };
        cx.try_global::<crate::terminal::git_status::GitStatusCache>()
            .and_then(|cache| cache.known_repo_for(view.host_id(), cwd))
            .flatten()
            .is_some()
    }

    /// "New Worktree Tab": probe the repository containing the tab's cwd for
    /// defaults (a fresh generated name, the current branch as start point) on
    /// the background executor, then open the confirmation sheet
    /// (`ui::worktree_prompt`) where name/branch/base can be edited before
    /// anything is created. Failure to probe lands as a notification.
    pub(crate) fn new_worktree_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((host, cwd)) = self.tab_host_cwd(index, window, cx) else {
            window.push_notification("This tab has no working directory yet", cx);
            return;
        };
        // The sheet keeps the host the defaults were probed from, so the create
        // it eventually submits cannot end up on a different machine than the
        // branch list it was filled from.
        let sheet_host = host.clone();
        let probe_cwd = cwd.clone();
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| crate::core::worktree::defaults(h, &probe_cwd),
            move |this, result, window, cx| match result {
                Ok(defaults) => this.open_worktree_prompt(sheet_host, cwd, defaults, window, cx),
                Err(e) => window.push_notification(format!("New worktree failed: {e}"), cx),
            },
        );
    }

    /// Open the tab for a just-created worktree: a default-shell terminal in
    /// the worktree directory, with the tab pre-named after its branch so a
    /// strip of parallel worktrees stays tellable-apart. Mirrors
    /// `new_tab_with_shell`, minus the cwd inheritance (the cwd *is* the point).
    pub(crate) fn open_worktree_tab(
        &mut self,
        wt: crate::core::worktree::NewWorktree,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = match new_terminal(
            self.window_workspace(cx),
            self.font_size,
            Some(wt.path),
            None,
            None,
            window,
            cx,
        ) {
            Ok(view) => view,
            Err(e) => {
                log::error!("worktree tab spawn failed: {e}");
                window.push_notification(format!("Could not open a terminal: {e}"), cx);
                return;
            }
        };
        self.remember_active_pane(window, cx);
        self.maximized = None;
        let insert_at = self.new_tab_insert_at(cx);
        let mut tab = Tab::new(Pane::leaf(view));
        tab.name = Some(wt.branch);
        self.tabs.insert(insert_at, tab);
        self.active = insert_at;
        self.focus_active(window, cx);
        self.save_session(cx);
        cx.notify();
    }

    /// Rearrange the whole tab vector into `order` (old indices in their new
    /// order) — the single path by which a drag-reorder lands, used by the
    /// sidebar where a single visual move can imply a larger permutation
    /// (relocating a tab inside its group without disturbing the group order,
    /// or moving an entire group). Keeps the same tab active and re-persists.
    pub(crate) fn apply_tab_order(&mut self, order: &[usize], cx: &mut Context<Self>) {
        if order.len() != self.tabs.len() || order.iter().enumerate().all(|(i, &o)| i == o) {
            return;
        }
        // Reordering shifts indices: a rename pending on a fixed one would
        // commit onto the wrong tab.
        self.renaming = None;
        let was_active = self.active;
        let mut slots: Vec<Option<Tab>> = std::mem::take(&mut self.tabs)
            .into_iter()
            .map(Some)
            .collect();
        self.tabs = order.iter().filter_map(|&i| slots[i].take()).collect();
        self.active = order.iter().position(|&i| i == was_active).unwrap_or(0);
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

    /// Turn the title-bar workspace chip into a text field, seeded with the
    /// current name. Committing on Enter or blur mirrors the tab rename.
    pub(crate) fn start_workspace_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = WorkspaceStore::all(cx)
            .get(self.workspace)
            .map(|w| w.display_name())
            .unwrap_or_default();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current));
        input.update(cx, |state, cx| state.focus(window, cx));
        let subs = vec![cx.subscribe_in(
            &input,
            window,
            |this, _input, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    this.commit_workspace_rename(window, cx)
                }
                _ => {}
            },
        )];
        self.workspace_rename = Some(WorkspaceRename { input, _subs: subs });
        cx.notify();
    }

    /// Commit the workspace rename. An empty value clears the custom name, so
    /// the chip falls back to the derived repo name — the same "clear to
    /// revert" contract the tab rename has.
    pub(crate) fn commit_workspace_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.workspace_rename.take() else {
            return;
        };
        let value = rename.input.read(cx).value().trim().to_string();
        let id = self.workspace;
        WorkspaceStore::rename(cx, id, (!value.is_empty()).then_some(value));
        crate::ui::windows::refresh_menu(cx);
        self.sync_window_title(window, cx);
        self.focus_active(window, cx);
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
        crate::ui::windows::refresh_menu(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    // ----- Command palette -------------------------------------------------

    /// Build the full command catalog: the static commands plus one
    /// "Switch to Tab: …" entry per open tab (label matches the tab strip).
    fn palette_commands(&self, cx: &App) -> Vec<Command> {
        // This window's own chrome state, not the config's copy of it — see
        // `ChromeState`.
        let mut commands = Command::base_commands(
            cx,
            ChromeState {
                rail_collapsed: self.sidebar_collapsed,
                right_panel_visible: self.right_panel_visible,
            },
        );

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
                .with_subtitle(subtitle)
                .in_group(CommandGroup::Ssh),
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
            commands.push(
                Command::new(
                    format!("Switch to Tab: {label}"),
                    CommandKind::ActivateTab(i),
                )
                .in_group(CommandGroup::TabsPanes),
            );
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

    /// The focused terminal of the active tab, for palette commands that act on
    /// the pane rather than the shell. The palette has already closed by the
    /// time these run, so focus is back where the user left it.
    fn focused_leaf(&self, window: &Window, cx: &App) -> Option<Entity<TerminalView>> {
        self.tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
    }

    /// Record that a palette command was run, for the palette's Recent band.
    /// Only commands with a stable id are tracked (see `CommandKind::id`).
    fn bump_command_frecency(&mut self, kind: &CommandKind, cx: &mut Context<Self>) {
        let Some(id) = kind.id() else { return };
        self.update_config(cx, |cfg| {
            let entry = cfg.command_frecency.entry(id.to_string()).or_default();
            entry.count = entry.count.saturating_add(1);
            entry.last_used = crate::core::config::unix_now();
        });
    }

    /// Run a palette command by dispatching to the matching tab/pane operation.
    fn run_command(&mut self, kind: CommandKind, window: &mut Window, cx: &mut Context<Self>) {
        use CommandKind::*;
        self.bump_command_frecency(&kind, cx);
        match kind {
            NewTab => self.new_tab(window, cx),
            NewWorkspace => crate::ui::windows::open(cx, None),
            OpenWorkspacePicker => self.open_switcher(window, cx),
            StopWorkspace => self.stop_workspace(self.workspace, window, cx),
            DeleteWorkspace => self.delete_workspace(self.workspace, window, cx),
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
            ToggleLeftPanel => self.toggle_left_panel(cx),
            ToggleRightPanel => self.toggle_right_panel(cx),
            ShowRightPanel(tab) => self.set_right_panel_tab(tab, cx),
            ResetFontSize => self.reset_font_size(cx),
            // Pane-scoped commands act on the terminal the closing palette just
            // handed focus back to.
            FindInTerminal => {
                if let Some(leaf) = self.focused_leaf(window, cx) {
                    leaf.update(cx, |view, cx| view.open_search(window, cx));
                }
            }
            FindNext => {
                if let Some(leaf) = self.focused_leaf(window, cx) {
                    leaf.update(cx, |view, cx| view.find_step(true, cx));
                }
            }
            FindPrevious => {
                if let Some(leaf) = self.focused_leaf(window, cx) {
                    leaf.update(cx, |view, cx| view.find_step(false, cx));
                }
            }
            ClearTerminal => {
                if let Some(leaf) = self.focused_leaf(window, cx) {
                    leaf.update(cx, |view, cx| view.clear_scrollback(cx));
                }
            }
            CopyText => {
                if let Some(leaf) = self.focused_leaf(window, cx) {
                    // `false`: a menu/palette copy leaves the highlight up. Only
                    // the dual-purpose ⌃C chord has to consume the selection.
                    leaf.update(cx, |view, cx| {
                        view.copy_contextual(false, cx);
                    });
                }
            }
            CutText => {
                if let Some(leaf) = self.focused_leaf(window, cx) {
                    leaf.update(cx, |view, cx| {
                        view.cut_contextual(cx);
                    });
                }
            }
            PasteText => {
                if let Some(leaf) = self.focused_leaf(window, cx) {
                    leaf.update(cx, |view, cx| view.paste_from_clipboard(cx));
                }
            }
            SelectAllText => {
                if let Some(leaf) = self.focused_leaf(window, cx) {
                    leaf.update(cx, |view, cx| view.select_all_contextual(cx));
                }
            }
            ReopenClosedTab => self.reopen_closed_tab(window, cx),
            RenameTab => self.start_rename(self.active, window, cx),
            NewWorktreeTab => self.new_worktree_tab(self.active, window, cx),
            CloseOtherTabs => self.close_other_tabs(self.active, window, cx),
            CloseTabsToTheRight => self.close_tabs_right_of(self.active, window, cx),
            CopyWorkingDirectory => self.copy_active_cwd(window, cx),
            MarkTabUnread => self.mark_tab_unread(self.active, cx),
            ForkAgentSession => self.fork_active_pane_session(ForkPlacement::NewTab, window, cx),
            CopyAgentSessionId => self.copy_agent_session_id(self.active, window, cx),
            RenameWorkspace => self.start_workspace_rename(window, cx),
            OpenSettings => self.toggle_settings(window, cx),
            ShowKeyboardShortcuts => {
                self.open_settings_section(SettingsSection::Keybindings, window, cx)
            }
            About => self.open_settings_section(SettingsSection::About, window, cx),
            CheckForUpdates => self.check_for_updates_now(window, cx),
            OpenDocumentation => cx.open_url(DOCS_URL),
            OpenDiscord => cx.open_url(DISCORD_URL),
            ReportIssue => cx.open_url(ISSUES_URL),
            Quit => cx.quit(),
            RestartDaemon => self.restart_daemon(window, cx),
            ToggleSftp => self.toggle_sftp(window, cx),
            ShowSshForwards => self.show_ssh_forwards(window, cx),
            ToggleCodePanel => self.toggle_code_panel(window, cx),
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
            SendSelectionToAgent => self.send_selection_to_agent(window, cx),
            SendGitDiffToAgent => self.send_git_diff_to_agent(window, cx),
            // Handled inside `PaletteView` (opens a sub-list); these never emit a
            // `Confirm` for this variant, so they never reach here.
            OpenThemePicker | OpenSshConnectInput => {}
            ActivateTab(i) => self.activate(i, window, cx),
        }
    }

    // ----- Agent context feed (palette: "Agent: …") -------------------------

    /// The pane the agent-feed commands deliver to: the first leaf running a
    /// recognized coding agent, preferring the active tab, then any tab. `None`
    /// when no agent runs anywhere.
    pub(crate) fn agent_target_leaf(&self, cx: &App) -> Option<Entity<TerminalView>> {
        let runs_agent = |leaf: &Entity<TerminalView>| leaf.read(cx).agent().is_some();
        if let Some(tab) = self.tabs.get(self.active)
            && let Some(leaf) = tab.pane.terminals().into_iter().find(runs_agent)
        {
            return Some(leaf);
        }
        self.tabs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.active)
            .flat_map(|(_, t)| t.pane.terminals())
            .find(runs_agent)
    }

    /// Deliver `prompt` into the agent pane's PTY and bring that pane's tab to
    /// the front so the user sees the turn start. Toasts when no agent runs.
    fn deliver_agent_prompt(&mut self, prompt: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.agent_target_leaf(cx) else {
            crate::terminal::notify_desktop(
                Some("tty7"),
                "No running coding agent found — start one (claude, codex, …) in a pane first.",
            );
            return;
        };
        target.read(cx).send_agent_prompt(prompt);
        if let Some(i) = self
            .tabs
            .iter()
            .position(|t| t.pane.terminals().contains(&target))
        {
            self.activate(i, window, cx);
        }
    }

    /// "Agent: Send Selection" — the focused pane's selection, phrased as a
    /// prompt, into the running agent's pane (the context-feed idea).
    fn send_selection_to_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let source = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx));
        let (selection, cwd) = match &source {
            Some(view) => (view.read(cx).selection_text(), view.read(cx).cwd()),
            None => (None, None),
        };
        let Some(selection) = selection else {
            crate::terminal::notify_desktop(
                Some("tty7"),
                "Nothing selected — select some terminal output first.",
            );
            return;
        };
        let cwd = cwd.map(|c| c.to_string_lossy().into_owned());
        if let Some(prompt) =
            crate::core::agent_prompt::build_selection_prompt(&selection, cwd.as_deref())
        {
            self.deliver_agent_prompt(&prompt, window, cx);
        }
    }

    /// "Agent: Send Git Diff for Review" — the focused pane's repo diff
    /// (unstaged + staged), phrased as a review prompt, into the agent's pane.
    fn send_git_diff_to_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx));
        // The pane's own host answers, so a pane whose repository lives on
        // another machine gets *its* diff rather than "no known directory".
        // What is still refused is a cwd no host in this process can answer for.
        let target = pane.and_then(|view| {
            let view = view.read(cx);
            Some((view.host(cx)?, view.host_cwd()?))
        });
        let Some((host, cwd)) = target else {
            crate::terminal::notify_desktop(Some("tty7"), "This pane has no known directory.");
            return;
        };
        // Off the UI thread, unlike before: two `git diff` runs against a big
        // repository froze the window for as long as they took, and against a
        // remote host they would be two round trips. This is the one visible
        // change — the menu item now returns immediately and the prompt is
        // delivered when the diff lands.
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| {
                // Unstaged + staged, concatenated — "everything not yet
                // committed", which is what a review pass wants. A failed
                // invocation contributes an empty string, exactly as it did
                // when this shelled out directly: a diff that cannot be read is
                // reported as "no uncommitted changes", not as an error.
                let run = |args: &[&str]| {
                    h.git(&cwd, args)
                        .ok()
                        .filter(|o| o.success())
                        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                        .unwrap_or_default()
                };
                let diff = format!("{}{}", run(&["diff"]), run(&["diff", "--cached"]));
                (diff, cwd.to_string_lossy().into_owned())
            },
            move |this, (diff, cwd_s), window, cx| {
                match crate::core::agent_prompt::build_diff_review_prompt(&diff, Some(&cwd_s)) {
                    Some(prompt) => this.deliver_agent_prompt(&prompt, window, cx),
                    None => crate::terminal::notify_desktop(
                        Some("tty7"),
                        &format!("No uncommitted changes in {cwd_s} (or not a git repository)."),
                    ),
                }
            },
        );
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
        let link_file_command_input = self.build_link_file_command_input(&mut subs, window, cx);
        let scroll_slider = self.build_scroll_slider(&mut subs, window, cx);
        let window_opacity_slider = self.build_window_opacity_slider(&mut subs, window, cx);
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

        // Live filter for the SSH section's host list; each keystroke re-renders
        // the master column so the list narrows as you type.
        let ssh_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter hosts…"));
        subs.push(
            cx.subscribe_in(&ssh_filter, window, |_this, _i, ev, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        );

        // The SSH empty state's quick-connect box. Its Connect button enables only
        // on a parsable target, so each keystroke re-renders the pane.
        let ssh_quick_connect =
            cx.new(|cx| InputState::new(window, cx).placeholder("user@host  or  user@host:port"));
        subs.push(
            cx.subscribe_in(&ssh_quick_connect, window, |_this, _i, ev, _w, cx| {
                if matches!(ev, InputEvent::Change) {
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
            link_file_command_input,
            scroll_slider,
            window_opacity_slider,
            theme_editor: None,
            theme_panel_open: false,
            theme_panel_slot: crate::ui::settings::ThemeSlot::Manual,
            theme_search,
            recording: None,
            rebinding_note: None,
            ssh_form: None,
            ssh_detail: crate::ui::settings::SshDetail::None,
            ssh_filter,
            ssh_collapsed_groups: std::collections::HashSet::new(),
            ssh_quick_connect,
            agent_hooks_host: crate::ui::host_ops::HostId::LOCAL,
            agent_hooks_states: crate::ui::settings::AgentHooksView::Loading,
            agent_hooks_seq: 0,
            agent_hooks_note: None,
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
        self.ensure_agent_hooks_loaded(cx);
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

    /// File-open command template input (Links section), committing on Enter/blur.
    fn build_link_file_command_input(
        &mut self,
        subs: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let value = cx
            .global::<Config>()
            .link_file_command
            .clone()
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("open in default app")
                .default_value(value)
        });
        subs.push(
            cx.subscribe_in(&input, window, move |this, _i, ev, _w, cx| {
                if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.commit_link_file_command(cx);
                }
            }),
        );
        input
    }

    /// Persist the file-open command template from the Links settings input. An
    /// empty value clears the override (falls back to the built-in open).
    fn commit_link_file_command(&mut self, cx: &mut Context<Self>) {
        let Some(command) = self.active_settings().map(|s| {
            s.link_file_command_input
                .read(cx)
                .value()
                .trim()
                .to_string()
        }) else {
            return;
        };
        let command = if command.is_empty() {
            None
        } else {
            Some(command)
        };
        let cfg = cx.global_mut::<Config>();
        if cfg.link_file_command == command {
            return; // no change — avoid a redundant disk write on every Blur
        }
        cfg.link_file_command = command;
        cfg.save();
        cx.notify();
    }

    /// Window-opacity slider for the Appearance page (20%–100%). Emits `Change`
    /// continuously as the user drags; each tick sets the global override and
    /// repaints, so the translucency is live under the thumb.
    fn build_window_opacity_slider(
        &mut self,
        subs: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SliderState> {
        let eff = Self::effective_window_opacity(cx);
        let slider = cx.new(|_| {
            SliderState::new()
                .min(0.2)
                .max(1.0)
                .step(0.01)
                .default_value(eff)
        });
        subs.push(
            cx.subscribe_in(&slider, window, |this, _s, ev: &SliderEvent, window, cx| {
                if let SliderEvent::Change(v) = ev {
                    this.set_window_opacity(v.start(), window, cx);
                }
            }),
        );
        slider
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
            for leaf in tab.pane.terminals() {
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
            for leaf in tab.pane.terminals() {
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
    fn reload_from_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Re-apply the theme with the window in hand: the watcher task calls
        // `apply_theme(None)` (colors/palette), but the window-bound effects — the
        // Transparent↔Blurred background flip and traffic-light re-pinning — only
        // happen here. Also keeps the Appearance opacity slider's thumb on a value
        // that was hand-edited in `config.json` or the theme file.
        apply_theme(Some(window), cx);
        self.sync_window_opacity_slider(window, cx);
        let config = cx.global::<Config>().clone();
        if config.cursor_style != self.terminal_cursor_style
            || config.scrollback_limit != self.terminal_scrollback_limit
        {
            self.terminal_cursor_style = config.cursor_style;
            self.terminal_scrollback_limit = config.scrollback_limit;
            self.apply_terminal_config_to_panes(&config, cx);
        }
        let (font_size, line_height, font_family, font_features) = {
            let cfg = cx.global::<Config>();
            (
                cfg.font_size,
                cfg.line_height,
                cfg.font_family.clone(),
                cfg.font_features
                    .as_ref()
                    .map(crate::core::config::gpui_font_features),
            )
        };
        // Keep the runtime sidebar width in step with the config (an external
        // edit to `config.json`, or our own drag-end persist which re-fires this).
        self.sidebar_width.set(cx.global::<Config>().sidebar_width);
        self.right_panel_width
            .set(cx.global::<Config>().right_panel_width);
        if font_size != self.font_size {
            self.font_size = font_size;
            let px_size = px(font_size);
            for tab in &self.tabs {
                for leaf in tab.pane.terminals() {
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
                for leaf in tab.pane.terminals() {
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
                for leaf in tab.pane.terminals() {
                    let family = font_family.clone();
                    leaf.update(cx, |v, cx| v.set_font_family(family, cx));
                }
            }
        }
        if font_features != self.font_features {
            self.font_features = font_features.clone();
            for tab in &self.tabs {
                for leaf in tab.pane.terminals() {
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
                for leaf in tab.pane.terminals() {
                    let bold = bold.clone();
                    leaf.update(cx, |v, cx| v.set_font_family_bold(bold, cx));
                }
            }
        }
        if italic != self.font_family_italic {
            self.font_family_italic = italic.clone();
            for tab in &self.tabs {
                for leaf in tab.pane.terminals() {
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
            for leaf in tab.pane.terminals() {
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
    /// session (PRD FR-E2), as an RGB value from the same hardcoded semantic
    /// palette as [`AgentStatus::dot_rgb`] — not the theme's UI tokens, which
    /// in this app are soft neutral fills (accent is the list-selection grey)
    /// and read as no state at all. Native panes are phase-coloured
    /// (connecting = amber, connected = green, failed/disconnected = red); a
    /// foreground `ssh` typed into a shell gets a plain neutral dot. `None`
    /// for non-SSH tabs (no dot).
    ///
    /// [`AgentStatus::dot_rgb`]: crate::core::cli_agent::AgentStatus::dot_rgb
    pub(crate) fn tab_ssh_dot(&self, tab: &Tab, cx: &App) -> Option<u32> {
        use crate::daemon::protocol::SshPhase;
        let leaf = tab.pane.first_leaf()?;
        // No dot for a pane still connecting: the SSH phase it would report is
        // the *pane's* SSH, and it has none yet.
        let v = leaf.terminal()?.read(cx);
        if let Some(phase) = v.ssh_phase() {
            // Native pane.
            let rgb = if v.ssh_disconnected() {
                0xEF4444 // red: link lost
            } else {
                match phase {
                    SshPhase::Connecting | SshPhase::Authenticating => 0xF59E0B, // amber: in flight
                    SshPhase::Connected => 0x22C55E,                             // green: link up
                    SshPhase::Failed { .. } => 0xEF4444, // red: never made it
                }
            };
            Some(rgb)
        } else if v
            .remote_context()
            .is_some_and(|r| r.kind != crate::daemon::protocol::RemoteKind::Wsl)
        {
            // A foreground `ssh` typed into a shell: a plain neutral dot. The
            // kind check matters: a WSL pane also carries a `RemoteContext` (so
            // its cwd is treated as foreign — see `local_cwd`), but it is not an
            // SSH session and this dot means "SSH".
            Some(0x9CA3AF)
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
            .map(|t| {
                t.pane
                    .terminals()
                    .iter()
                    .any(|l| self.leaf_is_warn_ssh(l, cx))
            })
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

    /// The focused pane when it is an SSH session of either kind.
    ///
    /// Not every pane carrying a `RemoteContext` is one: a WSL pane has one too,
    /// so that its cwd is treated as foreign (see `TerminalView::local_cwd`),
    /// and it must not reach anything SSH-shaped from here.
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
        let remote = pane.remote_context()?;
        (remote.kind != crate::daemon::protocol::RemoteKind::Wsl).then_some((pane.pane_id, remote))
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
            // Entering Agents re-reads the hook install states, so edits made
            // behind the panel's back (another tty7, a hand edit) show up.
            if target == SettingsSection::Agents {
                s.agent_hooks_states = crate::ui::settings::AgentHooksView::Loading;
            }
        }
        self.ensure_agent_hooks_loaded(cx);
        cx.notify();
    }

    /// Read the Agents page's rows, but only when that is the page on screen.
    ///
    /// Gated on the section because the read is a config file per agent — on a
    /// remote machine, a round trip per agent — and opening Settings on
    /// Appearance has no business paying for six of those.
    fn ensure_agent_hooks_loaded(&mut self, cx: &mut Context<Self>) {
        if self
            .active_settings()
            .is_some_and(|s| s.section == SettingsSection::Agents)
        {
            self.load_agent_hooks_states(cx);
        }
    }

    /// The machines the Agents section offers: this computer, then every remote
    /// machine this process is connected to right now.
    ///
    /// Only connected ones, because a hook install *is* a write to that
    /// machine's disk — there is nothing to offer without a link. The ones that
    /// are configured but offline are named under the picker instead of being
    /// silently dropped from it.
    pub(crate) fn agent_hooks_machines(
        &self,
        cx: &mut App,
    ) -> Vec<crate::ui::settings::AgentHooksMachine> {
        use crate::ui::settings::AgentHooksMachine;
        let mut out = vec![AgentHooksMachine {
            host: crate::ui::host_ops::HostId::LOCAL,
            label: "This Computer".to_string(),
        }];
        // The label is the name the user gave the box; `HostId` alone is a
        // hash. `available_hosts` is the same lookup the workspace switcher
        // does for exactly this reason.
        let configured = crate::ui::remote_connect::available_hosts(cx);
        for id in crate::ui::host_registry::HostRegistry::ids(cx) {
            if id.is_local() {
                continue;
            }
            let label = configured
                .iter()
                .find(|h| h.target.host_id() == id)
                .map(|h| h.label.clone())
                .unwrap_or_else(|| "Remote machine".to_string());
            out.push(AgentHooksMachine { host: id, label });
        }
        out
    }

    /// How many machines the Agents picker cannot offer because nothing is
    /// connected to them.
    ///
    /// Saved SSH profiles only — not the `~/.ssh/config` aliases
    /// [`available_hosts`](crate::ui::remote_connect::available_hosts) also
    /// returns. A config with fifty `Host` blocks is normal and most of them are
    /// git transports that could never host a workspace; counting those would
    /// turn a helpful footnote into "50 machines aren't connected".
    pub(crate) fn agent_hooks_offline_count(&self, cx: &mut App) -> usize {
        let connected = crate::ui::host_registry::HostRegistry::ids(cx);
        cx.global::<Config>()
            .ssh_profiles
            .iter()
            .filter(|p| {
                !connected
                    .contains(&crate::core::session::RemoteTarget::Profile { id: p.id }.host_id())
            })
            .count()
    }

    /// Point the Agents section at another machine and read its state.
    pub(crate) fn select_agent_hooks_host(
        &mut self,
        host: crate::ui::host_ops::HostId,
        cx: &mut Context<Self>,
    ) {
        if let Some(s) = self.settings.as_mut() {
            if s.agent_hooks_host == host {
                return;
            }
            s.agent_hooks_host = host;
            // The note belonged to the machine we just left.
            s.agent_hooks_note = None;
            s.agent_hooks_states = crate::ui::settings::AgentHooksView::Loading;
        }
        self.load_agent_hooks_states(cx);
        cx.notify();
    }

    /// Read every hook-capable agent's install state off the selected machine,
    /// in the background, and land the rows when they arrive.
    ///
    /// Background because a `Host` call blocks and on a remote machine that is a
    /// round trip *per agent* — six of them, on a link that may be an ocean
    /// wide. Doing it inline is the window freeze this codebase has already
    /// fixed twice.
    fn load_agent_hooks_states(&mut self, cx: &mut Context<Self>) {
        use crate::core::agent_hooks::{HookAgent, HookTarget};
        use crate::ui::settings::{AgentHookRow, AgentHooksView};

        let Some(host_id) = self.settings.as_ref().map(|s| s.agent_hooks_host) else {
            return;
        };
        let seq = match self.settings.as_mut() {
            Some(s) => {
                s.agent_hooks_seq += 1;
                s.agent_hooks_seq
            }
            None => return,
        };
        let Some((host, home)) = self.agent_hooks_link(host_id, cx) else {
            if let Some(s) = self.settings.as_mut() {
                s.agent_hooks_states =
                    AgentHooksView::Unavailable(Self::AGENT_HOOKS_OFFLINE.into());
            }
            cx.notify();
            return;
        };

        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| {
                let target = match &home {
                    Some(home) => HookTarget::remote(h, home.clone()),
                    None => HookTarget::local(h)?,
                };
                Some(
                    HookAgent::ALL
                        .into_iter()
                        .map(|agent| AgentHookRow {
                            agent,
                            state: crate::core::agent_hooks::hooks_state(&target, agent),
                            target: agent.target_display(&target),
                        })
                        .collect::<Vec<_>>(),
                )
            },
            move |this, rows, cx| {
                if let Some(s) = this.settings.as_mut()
                    && s.agent_hooks_seq == seq
                {
                    s.agent_hooks_states = match rows {
                        Some(rows) => AgentHooksView::Ready(rows),
                        None => AgentHooksView::Unavailable(
                            "tty7 could not work out this computer's home directory, so there is \
                             nowhere to install to."
                                .into(),
                        ),
                    };
                    cx.notify();
                }
            },
        );
    }

    /// What the Agents section says when the machine it is pointed at has no
    /// live connection. One string, because the picker's footnote and the
    /// rows' resting state have to agree.
    const AGENT_HOOKS_OFFLINE: &'static str = concat!(
        "Not connected to this machine, so its agent config can't be read or ",
        "written. Open a workspace on it and come back."
    );

    /// The host object and remote home for the machine the Agents section is
    /// pointed at, or `None` when it is a remote that is no longer connected.
    ///
    /// `None` for the home means "this computer" — the local target reads its
    /// own environment, which is the one place `$CLAUDE_CONFIG_DIR` and
    /// `$XDG_CONFIG_HOME` are ours to honor.
    fn agent_hooks_link(
        &self,
        host_id: crate::ui::host_ops::HostId,
        cx: &mut App,
    ) -> Option<(crate::ui::host_ops::SharedHost, Option<std::path::PathBuf>)> {
        let host = crate::ui::host_registry::HostRegistry::get(cx, host_id)?;
        if host_id.is_local() {
            return Some((host, None));
        }
        if !host.is_connected() {
            return None;
        }
        let home = crate::ui::remote_connect::RemoteConnections::home(cx, host_id)?;
        Some((host, Some(home)))
    }

    /// Settings → Agents: install (or rewrite in place) one agent's hooks on the
    /// selected machine, then fold the outcome back into the panel — status row
    /// + note line.
    pub(crate) fn settings_install_agent_hooks(
        &mut self,
        agent: crate::core::agent_hooks::HookAgent,
        cx: &mut Context<Self>,
    ) {
        self.run_agent_hooks_action(agent, true, cx);
    }

    /// Settings → Agents: remove one agent's tty7 hooks (user hooks survive).
    pub(crate) fn settings_uninstall_agent_hooks(
        &mut self,
        agent: crate::core::agent_hooks::HookAgent,
        cx: &mut Context<Self>,
    ) {
        self.run_agent_hooks_action(agent, false, cx);
    }

    /// Install or uninstall one agent's hooks on the selected machine, then
    /// re-read that machine's states — the ground truth, whatever the action
    /// just did — and surface the action's own summary or error as the note
    /// under its row.
    ///
    /// Writing is a `Host` call too, so it takes the same background trip as the
    /// read: an install into `~/.claude/settings.json` on a remote box is a read
    /// and a write over the control connection.
    fn run_agent_hooks_action(
        &mut self,
        agent: crate::core::agent_hooks::HookAgent,
        install: bool,
        cx: &mut Context<Self>,
    ) {
        use crate::core::agent_hooks::HookTarget;

        let Some(host_id) = self.settings.as_ref().map(|s| s.agent_hooks_host) else {
            return;
        };
        let Some((host, home)) = self.agent_hooks_link(host_id, cx) else {
            if let Some(s) = self.settings.as_mut() {
                s.agent_hooks_note = Some((agent, Self::AGENT_HOOKS_OFFLINE.to_string()));
                s.agent_hooks_states = crate::ui::settings::AgentHooksView::Unavailable(
                    Self::AGENT_HOOKS_OFFLINE.into(),
                );
            }
            cx.notify();
            return;
        };

        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            move |h| {
                let target = match &home {
                    Some(home) => HookTarget::remote(h, home.clone()),
                    None => HookTarget::local(h)
                        .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?,
                };
                if install {
                    crate::core::agent_hooks::install_hooks(&target, agent)
                } else {
                    crate::core::agent_hooks::uninstall_hooks(&target, agent)
                }
            },
            move |this, result, cx| {
                if let Some(s) = this.settings.as_mut() {
                    s.agent_hooks_note = Some((
                        agent,
                        match result {
                            Ok(summary) => summary,
                            Err(e) => format!("Failed: {e}"),
                        },
                    ));
                }
                this.load_agent_hooks_states(cx);
                cx.notify();
            },
        );
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

impl Tty7App {
    /// Design §10's status strip, on a window that has tabs.
    ///
    /// `ui::home` draws the same line on an *empty* remote window; this is the
    /// one that matters, because §17's rule — a window that loses its machine
    /// keeps showing what it had — only means anything when there is something
    /// to keep showing. Both read the same
    /// [`RemoteStatus::strip_message`](crate::ui::remote_workspace::RemoteStatus::strip_message),
    /// so the two surfaces cannot word it differently.
    ///
    /// Top-centre, deliberately away from the bottom notice: one says what is
    /// happening to the connection, the other what it means for the keyboard,
    /// and stacking them would read as one long apology.
    fn render_remote_workspace_strip(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.tabs.is_empty() {
            return None;
        }
        let status = self.remote_status(cx)?;
        let machine = self.remote_machine_label(cx);
        let message = status.strip_message(&machine)?;
        let action = status.action_label();
        let theme = cx.theme();
        let bar = gpui_component::h_flex()
            .occlude()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            .rounded_lg()
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .shadow_md()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(gpui_component::Icon::new(gpui_component::IconName::Globe))
            .child(
                div()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(message),
            )
            .when_some(action, |this, label| {
                use gpui_component::Sizable as _;
                use gpui_component::button::ButtonVariants as _;
                this.child(
                    gpui_component::button::Button::new("remote-status-action")
                        .label(label)
                        .primary()
                        .small()
                        .on_click(cx.listener(|this, _, _window, cx| this.remote_retry(cx))),
                )
            });
        Some(
            div()
                .absolute()
                .top_2()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(bar)
                .into_any_element(),
        )
    }

    /// Design §10's bottom line: 未连接 — 输入暂不生效.
    ///
    /// It exists because the degrade is otherwise invisible. Everything a
    /// disconnected window *can* still do — scroll, select, copy, ⌘F — works
    /// exactly as before, so the only observable difference is that typing
    /// stops doing anything, and a terminal that silently ignores keystrokes is
    /// indistinguishable from one that has hung.
    ///
    /// Not a place to offer buffering (D6): the notice says input has no effect
    /// because it has none, and a "queued" variant of this line would be a
    /// promise to replay keystrokes into a screen that has moved on.
    fn render_remote_input_notice(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.tabs.is_empty() {
            return None;
        }
        let notice = self.remote_status(cx)?.input_notice()?;
        let theme = cx.theme();
        Some(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_4()
                .flex()
                .justify_center()
                .child(
                    gpui_component::h_flex()
                        .occlude()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1p5()
                        .rounded_lg()
                        .bg(theme.popover)
                        .border_1()
                        .border_color(theme.warning.opacity(0.4))
                        .shadow_md()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(notice),
                )
                .into_any_element(),
        )
    }
}

/// Which SSH connection a forward is established on: the pane's own, or — for a
/// remote workspace — the **workspace's** (design §15, M7).
///
/// The same shape and the same reason as
/// [`SftpRoute`](crate::ui::sftp::SftpRoute): resolved on the UI thread from the
/// pane entity, then used wherever the request actually goes out. Both arms end
/// at the same `SshManager` on the local daemon; only the owner differs, and the
/// owner is what decides the lifetime — a pane's forwards die with the pane, a
/// workspace's outlive every pane in it.
///
/// **List, add and remove all take the same arm.** That is the property worth
/// protecting: a route that listed over the workspace and added over the pane
/// would produce a band you can read but not write, which is strictly worse than
/// the empty band a remote workspace showed before this existed.
#[derive(Clone, Debug, Default)]
pub(crate) struct ForwardRoute {
    pane_id: u64,
    /// `None` is the pane arm — an SSH pane, or a plain local shell.
    workspace: Option<crate::terminal::PaneWorkspace>,
}

impl ForwardRoute {
    fn workspace_op(
        &self,
        op: crate::daemon::protocol::WorkspaceOp,
    ) -> Option<crate::daemon::protocol::WorkspaceRequest> {
        crate::terminal::RemoteTerminal::workspace_request(
            self.workspace.as_ref()?,
            self.pane_id,
            op,
        )
    }

    /// A workspace reply, unwrapped to the forward list it should carry.
    ///
    /// A daemon that answers something else — most often `Error("workspace is
    /// not connected …")` — yields an empty list and a log line rather than a
    /// panic or a stale list: the band showing nothing is the truthful rendering
    /// of "this workspace's connection is gone".
    fn forwards(
        reply: anyhow::Result<crate::daemon::protocol::DaemonMsg>,
    ) -> Vec<crate::daemon::protocol::ManagedForward> {
        match reply {
            Ok(crate::daemon::protocol::DaemonMsg::ForwardList(list)) => list,
            Ok(other) => {
                log::warn!("unexpected reply to a workspace forward request: {other:?}");
                Vec::new()
            }
            Err(e) => {
                log::warn!("a workspace forward request failed: {e}");
                Vec::new()
            }
        }
    }

    pub(crate) fn list(&self) -> Vec<crate::daemon::protocol::ManagedForward> {
        let Some(req) = self.workspace_op(crate::daemon::protocol::WorkspaceOp::ListForwards)
        else {
            return crate::terminal::RemoteTerminal::list_forwards(self.pane_id);
        };
        Self::forwards(crate::terminal::RemoteTerminal::on_workspace(req))
    }

    pub(crate) fn add(
        &self,
        rule: crate::daemon::protocol::SshForwardRule,
    ) -> Vec<crate::daemon::protocol::ManagedForward> {
        let Some(req) = self
            .workspace_op(crate::daemon::protocol::WorkspaceOp::AddForward { rule: rule.clone() })
        else {
            return crate::terminal::RemoteTerminal::add_forward(self.pane_id, rule);
        };
        Self::forwards(crate::terminal::RemoteTerminal::on_workspace(req))
    }

    /// Drop every forward the workspace owns (design §15: they outlive the
    /// panes, so something has to end them when the workspace does).
    ///
    /// A no-op on the pane arm, and that is correct rather than a gap: a pane's
    /// forwards die with the pane through the daemon's own
    /// `teardown_pane_forwards`, so there is nothing here to duplicate.
    pub(crate) fn teardown(&self) -> Vec<crate::daemon::protocol::ManagedForward> {
        let Some(req) = self.workspace_op(crate::daemon::protocol::WorkspaceOp::TeardownForwards)
        else {
            return Vec::new();
        };
        Self::forwards(crate::terminal::RemoteTerminal::on_workspace(req))
    }

    pub(crate) fn remove(&self, forward_id: u64) -> Vec<crate::daemon::protocol::ManagedForward> {
        let Some(req) =
            self.workspace_op(crate::daemon::protocol::WorkspaceOp::RemoveForward { forward_id })
        else {
            return crate::terminal::RemoteTerminal::remove_forward(self.pane_id, forward_id);
        };
        Self::forwards(crate::terminal::RemoteTerminal::on_workspace(req))
    }
}

impl Render for Tty7App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A live drag-reorder commits when the drag *ends*, which in gpui means
        // the mouse was released (nothing else clears an active drag): the first
        // frame without one retires the preview and applies the order it was
        // last showing. Deliberately not an `on_drop` handler — those only fire
        // when the pointer is over that particular element at release, so a
        // release a hair outside the rail or the strip would silently lose the
        // move. What you were looking at is what you get, wherever you let go.
        // One place at the root covers every surface that can start a drag.
        if cx.has_active_drag() {
            // Still dragging: forget last frame's answer so only what this
            // frame actually draws can be committed (see `clear_pending`).
            crate::ui::reorder::clear_pending(&self.reorder);
        } else if let Some(order) = crate::ui::reorder::take_pending(&self.reorder) {
            self.apply_tab_order(&order, cx);
        }
        // While a tab or group is in hand, the cursor is a closed hand for the
        // whole window. It has to be set on the *drag* rather than styled on
        // the element: gpui overrides every hovered element's cursor with the
        // active drag's for the duration, and that override is `None` — a plain
        // arrow — unless something fills it in. Set once per drag (it forces a
        // refresh, so re-setting it every frame would spin).
        if self.reorder.borrow().is_some()
            && cx.active_drag_cursor_style() != Some(gpui::CursorStyle::ClosedHand)
        {
            cx.set_active_drag_cursor_style(gpui::CursorStyle::ClosedHand, window);
        }
        // Vertical-tab mode: the sidebar owns the tab list, so the title-bar strip
        // drops its chips (keeping only "+"/"⋯"). Gated on having tabs — the
        // zero-tab home page keeps the full-width horizontal layout, so an empty
        // rail never appears.
        let vertical = matches!(cx.global::<Config>().tab_bar_position, TabBarPosition::Left)
            && !self.tabs.is_empty();
        // The rail can be collapsed away without leaving `Left` mode. When it is,
        // the layout below has no left column, so the title strip takes over the
        // rail's jobs: it reserves the traffic lights and carries the sidebar's
        // own controls (new tab + expand) at its left edge.
        let rail = vertical && !self.sidebar_collapsed;
        let strip = self.tab_strip(!vertical, window, cx);
        let sidebar = rail.then(|| self.tab_sidebar(window, cx));
        // Native-SSH status strip / reconnect notice for the focused pane (E1/E4).
        let ssh_status = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
            .and_then(|leaf| self.render_ssh_status_strip(&leaf, cx));
        // Render the active tab's pane tree.
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
                        // Fading the unfocused panes only says anything once the
                        // tab is actually split, and the user can turn it off.
                        let dim_inactive = active_tab.pane.leaves().len() > 1
                            && cx.global::<Config>().dim_inactive_panes;
                        active_tab.pane.render(dim_inactive, window, cx)
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
            // Nothing of the SSH tooling floats over the terminal any more: port
            // forwarding is a band on the detail panel's Info tab, the remote file
            // browser is its Files tab, and transfers are the panel's footer. That
            // also gives the ⌘F find bar the top-right slot back — it used to have
            // to fight the tunnel/SFTP icons for it.
            //
            // In-pane native-SSH auth / host-key sheet (WS3), shown over the pane
            // that raised the prompt.
            .when_some(self.render_ssh_prompt_overlay(window, cx), |this, el| {
                this.child(el)
            })
            // Native-SSH status strip / reconnect notice (E1/E4).
            .when_some(ssh_status, |this, el| this.child(el))
            // The remote *workspace*'s own state (design §10). A sibling of the
            // SSH pane strip rather than a merge: that one is about one pane's
            // ssh process, this is about the machine the whole window is on, and
            // a window can legitimately show both.
            .when_some(self.render_remote_workspace_strip(cx), |this, el| {
                this.child(el)
            })
            .when_some(self.render_remote_input_notice(cx), |this, el| {
                this.child(el)
            })
            // Live-SSH close-confirmation sheet (E3).
            .when_some(self.render_ssh_close_confirm_overlay(cx), |this, el| {
                this.child(el)
            })
            // "New Worktree Tab" confirmation sheet (from the tab context menu).
            .when_some(self.render_worktree_prompt_overlay(cx), |this, el| {
                this.child(el)
            });

        // Working-tree diff overlay — mounted on the *column*, not on
        // `body_area`, so it covers the title strip too and reads as one
        // surface the way the code overlay does. Like that overlay it stops at
        // the rail and the right panel (both are siblings), which is the point:
        // the sidebar's git lines stay clickable to switch repo, and the
        // Changes list stays put so you can walk down it file by file.
        let diff_overlay = self.render_diff_overlay(cx);

        // Code panel: an immersive overlay ([file tree | editor], IDE-style)
        // covering the title strip *and* the terminal — the whole column right
        // of the tab sidebar — so nothing of the terminal chrome distracts.
        // The terminal underneath keeps its size (no PTY resize/reflow), and
        // the sidebar stays visible: switching tabs re-roots the tree.
        let code_overlay = self.render_code_overlay(window, cx);

        // The two column overlays, ordered so the one the user last acted on is
        // the later child and therefore paints on top. Neither outranks the
        // other by construction.
        let overlays: Vec<gpui::AnyElement> = {
            let mut pair = vec![
                (OverlayTop::Diff, diff_overlay),
                (OverlayTop::Code, code_overlay),
            ];
            if self
                .tabs
                .get(self.active)
                .is_some_and(|t| t.overlay_top == OverlayTop::Diff)
            {
                pair.reverse();
            }
            pair.into_iter().filter_map(|(_, el)| el).collect()
        };

        // The layout. The rail (vertical mode) is a full-height *left column* that
        // reaches the very top of the window — the traffic lights sit on its
        // surface — with the title strip and terminal stacked in the right column.
        // That way the rail surface has no seam with the title bar and reads as one
        // continuous panel.
        //
        // The right detail panel does the same on macOS: a full-height column
        // carrying its own title-bar-height top zone (tab row + the window's corner
        // chrome), so its surface runs unbroken from the very top of the window.
        //
        // Off macOS it can't. The window controls (─ ▢ ✕) are laid out by the title
        // bar itself, at *its* right end, so a full-height panel beside the bar
        // strands them mid-window with the panel's grey to their right. There the
        // bar spans the panel too — reaching the real top-right corner, where
        // Windows and Linux users expect the controls — and the panel hangs below
        // it, VS Code style. The panel's tab row then sits on the panel (no longer
        // in the caption row), and the corner chrome stays in the strip.
        let right_panel = self.render_right_panel(window, cx);
        let panel_below_title_bar = right_panel.is_some() && !cfg!(target_os = "macos");
        // Which host the bar goes to: the terminal column's first child, or the
        // spanning row above [terminal | panel].
        let (column_title_bar, spanning_title_bar) = if panel_below_title_bar {
            (None, Some(title_bar))
        } else {
            (Some(title_bar), None)
        };
        // And where the overlays hang. Normally on the terminal column, which they
        // fill: the bar is that column's first child, so an `inset_0` overlay
        // covers it and the overlay's own header row lands *on* the caption line —
        // which is what both headers are drawn for (title-bar height, the bar's
        // insets, a full-size chrome tile for their one control).
        //
        // With the bar hoisted, a column-anchored overlay starts 40px down and its
        // header sits one row too low: level with the panel's tab row instead of
        // with the caption. So it hangs on the row that owns the bar instead,
        // inset from the right by the panel's width — covering the bar's band over
        // the terminal column (which carries nothing there but the drag region, or
        // the rail's controls while it's collapsed: exactly what an overlay covers
        // with the panel closed) and stopping short of the panel, so the ─ ▢ ✕
        // group and the corner chrome keep their own surface and their clicks.
        let (column_overlays, hoisted_overlays) = if panel_below_title_bar {
            (Vec::new(), overlays)
        } else {
            (overlays, Vec::new())
        };
        let panel_px = self.right_panel_px(window, cx);
        // The terminal column, and the anchor for both overlays: they fill it —
        // and, since the panel is a sibling rather than a child, stop short of the
        // panel for free. With the bar spanning above, they stop short of it too,
        // which keeps the native controls clickable while an overlay is open and
        // lines the overlay's own header row up with the panel's tab row.
        let terminal_column = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .relative()
            .when_some(column_title_bar, |this, bar| this.child(bar))
            .child(body_area)
            .children(column_overlays);
        let panel_row = div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_row()
            .child(terminal_column)
            .when_some(right_panel, |this, panel| this.child(panel));
        let main_layout = div()
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .flex_row()
            .when_some(sidebar, |this, sidebar| this.child(sidebar))
            .child(match spanning_title_bar {
                Some(bar) => div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    // The containing block for the hoisted overlays below.
                    .relative()
                    .child(
                        // The bar's own band over the panel, painted in the panel's
                        // surface so the column still reads as one continuous
                        // sidebar from the very top of the window — the rail's
                        // trick, kept now that the tab row moved off the caption
                        // line. Without it the panel's grey started 40px down and
                        // the corner tore into two colours.
                        //
                        // A sibling *under* the transparent bar rather than padding
                        // inside it: the ─ ▢ ✕ group is the bar's own last child, so
                        // nothing laid out in the bar can get behind the controls,
                        // and only a layer below can carry a surface under them.
                        // Same width and left border as the panel, both read from
                        // `right_panel_px`, so the edge stays in register through a
                        // resize drag.
                        div()
                            .relative()
                            .flex_none()
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .bottom_0()
                                    .right_0()
                                    .w(px(self.right_panel_px(window, cx)))
                                    .bg(cx.theme().sidebar)
                                    .border_l_1()
                                    .border_color(cx.theme().sidebar_border),
                            )
                            .child(bar),
                    )
                    .child(panel_row)
                    // Last child, so they paint over both the bar and the column.
                    // Each overlay is `absolute().inset_0()` against this wrapper,
                    // which is the only thing that has to know where the panel
                    // starts.
                    .children(hoisted_overlays.into_iter().map(|overlay| {
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .bottom_0()
                            .right(px(panel_px))
                            .child(overlay)
                    }))
                    .into_any_element(),
                None => panel_row.into_any_element(),
            })
            .into_any_element();

        // The real window background paint: gradient-aware and opacity-carrying
        // (see `theme::window_background`), plus the theme's optional background
        // image. Falls back to the component theme's solid before the first
        // `apply_theme` has published the global.
        let (window_bg, bg_image) = match cx.try_global::<crate::ui::presets::ActiveBackground>() {
            Some(bg) => (window_background(bg), bg.image.clone()),
            None => (cx.theme().background.into(), None),
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
                // Same gradient-aware paint as the root, so a gradient theme's
                // settings page doesn't snap to a flat color. A translucent
                // theme's alpha rides along, letting the background image show
                // through here too. This second layer compounds the alpha over
                // the root's paint — deliberate: the overlay must occlude the
                // terminal behind it to stay readable.
                .bg(window_bg)
                .child(self.render_settings(cx))
        });

        div()
            .id("tty7-root")
            .size_full()
            .flex()
            .flex_col()
            .bg(window_bg)
            .text_color(cx.theme().foreground)
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.new_tab(window, cx)))
            .on_action(cx.listener(|this, _: &SelectWorkspace1, window, cx| {
                this.select_workspace_slot(0, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace2, window, cx| {
                this.select_workspace_slot(1, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace3, window, cx| {
                this.select_workspace_slot(2, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace4, window, cx| {
                this.select_workspace_slot(3, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace5, window, cx| {
                this.select_workspace_slot(4, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace6, window, cx| {
                this.select_workspace_slot(5, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace7, window, cx| {
                this.select_workspace_slot(6, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace8, window, cx| {
                this.select_workspace_slot(7, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectWorkspace9, window, cx| {
                this.select_workspace_slot(8, window, cx)
            }))
            .on_action(cx.listener(|this, _: &RenameWorkspace, window, cx| {
                this.start_workspace_rename(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ToggleSwitcher, window, cx| {
                    this.toggle_switcher(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &StopWorkspace, window, cx| {
                let id = this.workspace;
                this.stop_workspace(id, window, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteWorkspace, window, cx| {
                let id = this.workspace;
                this.delete_workspace(id, window, cx);
            }))
            .on_action(cx.listener(|_this, _: &NewWorkspace, _window, cx| {
                // A fresh workspace, not a copy of this one: the daemon gives
                // each pane a single subscriber, so a second window onto the
                // same panes would steal this window's output.
                crate::ui::windows::open(cx, None);
            }))
            .on_action(cx.listener(|this, _: &CloseActiveTab, window, cx| {
                // With focus in the editor panel, ⌘W closes the active file
                // tab instead of the terminal pane/tab.
                if !this.editor_close_active_if_focused(window, cx) {
                    this.close_pane(window, cx)
                }
            }))
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
                cx.listener(|this, _: &ActivateTab1, window, cx| {
                    this.activate_visual(0, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab2, window, cx| {
                    this.activate_visual(1, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab3, window, cx| {
                    this.activate_visual(2, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab4, window, cx| {
                    this.activate_visual(3, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab5, window, cx| {
                    this.activate_visual(4, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab6, window, cx| {
                    this.activate_visual(5, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab7, window, cx| {
                    this.activate_visual(6, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab8, window, cx| {
                    this.activate_visual(7, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &ActivateTab9, window, cx| {
                    this.activate_visual(8, window, cx)
                }),
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
                cx.listener(|this, _: &ToggleLeftPanel, _window, cx| this.toggle_left_panel(cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleRightPanel, _window, cx| this.toggle_right_panel(cx)),
            )
            .on_action(cx.listener(|this, _: &ShowRightPanelInfo, _window, cx| {
                this.set_right_panel_tab(crate::core::config::RightPanelTab::Info, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowRightPanelOutline, _window, cx| {
                this.set_right_panel_tab(crate::core::config::RightPanelTab::Outline, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowRightPanelChanges, _window, cx| {
                this.set_right_panel_tab(crate::core::config::RightPanelTab::Changes, cx)
            }))
            .on_action(cx.listener(|this, _: &ShowRightPanelFiles, _window, cx| {
                this.set_right_panel_tab(crate::core::config::RightPanelTab::Files, cx)
            }))
            .on_action(
                cx.listener(|this, _: &OpenSettings, window, cx| this.toggle_settings(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &RestartDaemon, window, cx| this.restart_daemon(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleSftp, window, cx| this.toggle_sftp(window, cx)))
            .on_action(cx.listener(|this, _: &ShowSshForwards, window, cx| {
                this.show_ssh_forwards(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleCodePanel, window, cx| {
                this.toggle_code_panel(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &EditorSave, window, cx| this.editor_save_active(window, cx)),
            )
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
            // Tab operations that used to be reachable only by right-clicking a
            // chip. Each targets the active tab, so the menu bar / palette /
            // keyboard all mean "this tab" without a click to say which.
            .on_action(cx.listener(|this, _: &RenameTab, window, cx| {
                this.start_rename(this.active, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NewWorktreeTab, window, cx| {
                this.new_worktree_tab(this.active, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CloseOtherTabs, window, cx| {
                this.close_other_tabs(this.active, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CloseTabsToTheRight, window, cx| {
                this.close_tabs_right_of(this.active, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CopyWorkingDirectory, window, cx| {
                this.copy_active_cwd(window, cx)
            }))
            .on_action(cx.listener(|this, _: &MarkTabUnread, _window, cx| {
                this.mark_tab_unread(this.active, cx)
            }))
            // Fork: the bare action has no pane the user pointed at, so it
            // opens a new tab; the four directional ones come from the pane
            // right-click menu, where the ask *was* spatial.
            .on_action(cx.listener(|this, _: &ForkAgentSession, window, cx| {
                this.fork_active_pane_session(ForkPlacement::NewTab, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ForkAgentSessionRight, window, cx| {
                this.fork_focused_pane_session(Axis::Horizontal, false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ForkAgentSessionLeft, window, cx| {
                this.fork_focused_pane_session(Axis::Horizontal, true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ForkAgentSessionDown, window, cx| {
                this.fork_focused_pane_session(Axis::Vertical, false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ForkAgentSessionUp, window, cx| {
                this.fork_focused_pane_session(Axis::Vertical, true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CopyAgentSessionId, window, cx| {
                this.copy_agent_session_id(this.active, window, cx)
            }))
            // Settings destinations that deserve their own way in: Help →
            // Keyboard Shortcuts and the App menu's About both used to require
            // opening Settings and then hunting for the section.
            .on_action(cx.listener(|this, _: &ShowKeyboardShortcuts, window, cx| {
                this.open_settings_section(SettingsSection::Keybindings, window, cx)
            }))
            .on_action(cx.listener(|this, _: &About, window, cx| {
                this.open_settings_section(SettingsSection::About, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CheckForUpdates, window, cx| {
                this.check_for_updates_now(window, cx)
            }))
            // Standard macOS App / Window menu items. gpui exposes the platform
            // calls but ships no actions for them.
            .on_action(cx.listener(|_, _: &HideApp, _window, cx| cx.hide()))
            .on_action(cx.listener(|_, _: &HideOthers, _window, cx| cx.hide_other_apps()))
            .on_action(cx.listener(|_, _: &ShowAll, _window, cx| cx.unhide_other_apps()))
            .on_action(cx.listener(|_, _: &MinimizeWindow, window, _cx| window.minimize_window()))
            .on_action(cx.listener(|_, _: &ZoomWindow, window, _cx| window.zoom_window()))
            // Help destinations. Opened in the default browser; a failure here is
            // not worth interrupting the user over, so it is logged, not toasted.
            .on_action(cx.listener(|_, _: &OpenDocumentation, _window, cx| cx.open_url(DOCS_URL)))
            .on_action(cx.listener(|_, _: &OpenDiscord, _window, cx| cx.open_url(DISCORD_URL)))
            .on_action(cx.listener(|_, _: &ReportIssue, _window, cx| cx.open_url(ISSUES_URL)))
            // The theme's background image, composited over the background fill
            // at its own opacity and under all content. Absolute, so it doesn't
            // participate in the flex column; the wrapper clips the Cover
            // overflow (gpui's `img` paints the fitted bounds unclipped).
            .when_some(bg_image, |this, image| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .overflow_hidden()
                        .opacity(image.opacity)
                        .child(
                            img(image.path)
                                .size_full()
                                .object_fit(gpui::ObjectFit::Cover),
                        ),
                )
            })
            .child(main_layout)
            // Settings overlay, above the tabs/terminal when open.
            .when_some(settings_overlay, |this, overlay| this.child(overlay))
            // The workspace switcher, in the same layer as the palette: they
            // answer two different questions and are never open at once.
            .children(self.render_switcher(cx))
            // Command palette overlay, layered above everything when open.
            .when_some(self.palette.clone(), |this, palette| this.child(palette))
            // Toast layer for `window.push_notification` (worktree/SSH errors).
            // gpui-component's Root only *stores* the list; the root view must
            // render the layer — without this child every toast was invisible.
            .children(gpui_component::Root::render_notification_layer(window, cx))
    }
}

/// Convert a live `Tab` (pane tree + name) into its serializable mirror.
fn tab_to_session(tab: &Tab, cx: &App) -> SessionTab {
    SessionTab {
        name: tab.name.clone(),
        pane: pane_to_session(&tab.pane, cx),
        sidebar_group: tab.sidebar_group.borrow().clone(),
    }
}

/// Convert a live `Pane` tree into its serializable mirror, reading each
/// leaf's current cwd and each split's axis + ratio. Used when saving.
fn pane_to_session(pane: &Pane, cx: &App) -> SessionPane {
    match pane {
        // A pane still connecting has no terminal to interrogate — but it does
        // know which pane it is trying to re-attach to, and that is the whole
        // of what restore needs. Quitting mid-connect therefore comes back to
        // the same pane rather than dropping it from the layout.
        Pane::Leaf(PaneSlot::Connecting(pending)) => {
            let spawn = &pending.read(cx).spawn;
            SessionPane::Leaf {
                cwd: spawn.working_directory.clone(),
                pane_id: spawn.restore_pane,
                ssh_spec: None,
                agent: None,
                agent_session_id: None,
                agent_launch_argv: None,
            }
        }
        Pane::Leaf(PaneSlot::Ready(view)) => {
            let view = view.read(cx);
            SessionPane::Leaf {
                // A restored pane whose daemon pane is gone respawns through
                // `new_terminal(workspace, …)` — on the *same* machine, since
                // restore carries the window's workspace — so a remote
                // workspace's cwd is right to keep. What must not be kept is a
                // native-SSH or WSL pane's: those come back on the default
                // *local* shell (a shell pick isn't persisted), which cannot
                // chdir into the other machine's path. Native-SSH panes
                // reconnect from `ssh_spec` and the daemon discards the cwd for
                // them anyway (`server::SpawnNativeSsh`).
                cwd: view.spawnable_cwd(),
                pane_id: Some(view.pane_id),
                // Persist the secret-free native-SSH spec so a *dead* pane can be
                // reconnected on restore (FR-E4/C2); `None` for local panes. A
                // live pane reattaches by `pane_id` and never needs this.
                ssh_spec: view.ssh_spec(),
                // The running agent + its native session id (when its hooks
                // reported one), so a pane the daemon loses can resume the
                // agent conversation instead of just reopening a shell. The
                // observed launch argv rides along so the resume command keeps
                // the user's flags (`--dangerously-skip-permissions`, …).
                agent: view.agent(),
                agent_session_id: view.agent_session().and_then(|s| s.session_id),
                agent_launch_argv: view.agent_session().and_then(|s| s.launch_argv),
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
            agent: None,
            agent_session_id: None,
            agent_launch_argv: None,
        },
    }
}

/// Set of pane ids currently alive **on `route`'s machine**, used by
/// `session_to_pane` to decide per leaf whether to re-`attach` or `spawn`.
/// Computed once per restore from that daemon's `List`; empty (→ all-fresh)
/// when it is unreachable.
///
/// There is deliberately no unrouted sibling. Pane ids are **per daemon**:
/// asking this machine's daemon which of a remote workspace's ids are alive
/// answers about whatever local panes happen to hold those numbers. At restore
/// that is not a cosmetic error — a leaf would `Attach` to a stranger's pane and
/// put their shell on screen — and the display sites that used to take the
/// unrouted answer now go through
/// [`pane_liveness`](crate::terminal::pane_liveness), which cannot spell the
/// question without naming a machine.
pub(crate) fn alive_panes_on(route: &crate::terminal::PaneRoute) -> std::collections::HashSet<u64> {
    // **Local routes only.** This is a *blocking* `List`, and every caller is on
    // the UI thread — which is fine against a socket on this machine and is a
    // multi-second window freeze against one that has to open an SSH channel
    // first. A remote workspace answers the same question per pane instead, in
    // the background half of its spawn: `start_pane_spawn` tries the attach and
    // falls back to a fresh pane when the id is gone, which is exactly what
    // this set was being consulted for.
    if !matches!(route, crate::terminal::PaneRoute::Local) {
        return std::collections::HashSet::new();
    }
    crate::terminal::RemoteTerminal::list_panes_on(route)
        .into_iter()
        .filter(|p| p.alive)
        .map(|p| p.pane_id)
        .collect()
}

/// Rebuild the tab list from a persisted `Session`, re-attaching to still-live
/// daemon panes where possible and spawning fresh shells otherwise. An absent or
/// empty session yields no tabs (the home page). Shared by first-launch restore
/// (`Tty7App::for_workspace`) and the daemon-restart rebuild (`restart_daemon`), so the two
/// stay in lockstep.
fn tabs_from_session(
    workspace: Option<&crate::terminal::PaneWorkspace>,
    session: Option<Session>,
    font_size: f32,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> (Vec<Tab>, usize) {
    let Some(session) = session.filter(|s| !s.tabs.is_empty()) else {
        return (Vec::new(), 0);
    };
    // Ask *this workspace's* daemon once which panes are still alive, so leaves
    // re-attach to surviving shells instead of all spawning fresh.
    let alive = alive_panes_on(&crate::terminal::PaneRoute::for_workspace(workspace));
    let mut tabs: Vec<Tab> = Vec::with_capacity(session.tabs.len());
    for st in &session.tabs {
        // A tab whose every leaf failed to come back has nothing to show; drop
        // it rather than restore an empty frame (or, worse, abort the launch).
        let Some(pane) = session_to_pane(workspace, &st.pane, &alive, font_size, window, cx) else {
            log::error!("dropping a restored tab: no pane in it could be started");
            continue;
        };
        tabs.push(Tab {
            pane,
            name: st.name.clone(),
            last_focused: None,
            diff_overlay: None,
            code: None,
            overlay_top: OverlayTop::default(),
            // Seed the sticky group from the saved session so the sidebar
            // renders grouped on the first frame; the first landed probe
            // corrects it if the tab's repo changed while we were gone.
            sidebar_group: std::cell::RefCell::new(st.sidebar_group.clone()),
        });
    }
    // Clamp the saved active index into the rebuilt range (which can be empty
    // when nothing restored).
    let active = session.active.min(tabs.len().saturating_sub(1));
    (tabs, active)
}

/// Whether a restored leaf's saved `pane_id` names a pane in the same daemon
/// the caller read its `alive` set from — the window's daemon.
///
/// Pane ids are unique only *within* a daemon, so the question is not academic:
/// looking one up in the wrong set is how a saved id silently matches somebody
/// else's live pane and the restore attaches to it.
///
/// A native-SSH leaf is the one case where a pane does not live in its window's
/// daemon. Its russh session is spawned by **this client's** daemon however the
/// window is bound, so in a remote workspace it belongs to a different machine
/// than every other leaf around it — and its id must not be matched against the
/// remote's pane list. It reconnects from its saved spec instead, which is what
/// it does for any id that is no longer live.
fn leaf_shares_the_window_daemon(window_is_remote: bool, leaf_is_native_ssh: bool) -> bool {
    !(window_is_remote && leaf_is_native_ssh)
}

/// Rebuild a live `Pane` tree from a saved `SessionPane`. A leaf whose saved
/// `pane_id` is still alive in the daemon re-`attach`es (process + scrollback
/// intact); otherwise it spawns a fresh shell in the saved cwd. `alive` is the
/// daemon's current pane set, computed once by the caller.
///
/// `None` when nothing under this node could be started (an unreachable
/// daemon): restore drops what it can't rebuild instead of leaving `Empty`
/// nodes — which every tree operation ignores — in a live tab.
fn session_to_pane(
    workspace: Option<&crate::terminal::PaneWorkspace>,
    sp: &SessionPane,
    alive: &std::collections::HashSet<u64>,
    font_size: f32,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> Option<Pane> {
    match sp {
        SessionPane::Leaf {
            cwd,
            pane_id,
            ssh_spec,
            agent,
            agent_session_id,
            agent_launch_argv,
        } => {
            // Only restore the pane id when the daemon confirms it's still live;
            // a stale id (daemon restarted, pane killed) falls back to a spawn.
            //
            // …and `alive` is *one* daemon's pane set, so a leaf whose pane
            // lives in a different one must not be looked up in it.
            let same_daemon =
                leaf_shares_the_window_daemon(workspace.is_some(), ssh_spec.is_some());
            let restore = match workspace.is_some() {
                // A remote leaf keeps its id unconditionally: `alive` is empty
                // for a remote route by construction (see `alive_panes_on`), and
                // the attempt to attach happens off the UI thread, where a dead
                // id costs one failed round trip and falls back to a spawn.
                true => (*pane_id).filter(|_| same_daemon),
                false => (*pane_id).filter(|id| same_daemon && alive.contains(id)),
            };
            // A *dead* native-SSH leaf (spec persisted, pane no longer alive)
            // reconnects rather than dropping back to a local shell (FR-C2/E4):
            // re-resolve secrets from the profile when it names one, else reuse
            // the secret-free spec and let the auth sheets prompt.
            if restore.is_none() {
                if let Some(spec) = ssh_spec.clone() {
                    let resolved = crate::ui::ssh_connect::resolve_persisted_ssh_spec(spec, cx);
                    match new_terminal_native(font_size, cwd.clone(), resolved, window, cx) {
                        Ok(view) => return Some(Pane::leaf(PaneSlot::Ready(view))),
                        // Keep restore alive: fall through to a local shell in
                        // this slot rather than aborting startup.
                        Err(e) => log::error!("restoring native SSH pane failed: {e}"),
                    }
                }
            }
            // A shell pick isn't persisted in the session, so a stale pane that
            // must respawn comes back on the default shell.
            let view = match new_terminal(
                workspace.cloned(),
                font_size,
                cwd.clone(),
                restore,
                None,
                window,
                cx,
            ) {
                Ok(view) => view,
                Err(e) => {
                    log::error!("restoring pane failed: {e}");
                    return None;
                }
            };
            // A pane that could NOT re-attach lost its running agent with the
            // daemon; when we captured that agent's native session id, hand
            // the fresh shell its resume command so the conversation picks up
            // where it left off (cmux's auto-resume, config-gated). The bytes
            // sit in the PTY input queue until the shell reads its first
            // command — same mechanism as tmux send-keys at spawn.
            if restore.is_none()
                && cx.global::<Config>().restore_agent_sessions
                && let (Some(agent), Some(id)) = (agent, agent_session_id)
                && let Some(cmd) = agent.resume_command(id, agent_launch_argv.as_deref())
                // A pane whose terminal has not arrived yet cannot be sent a
                // resume command. It does not need one either: `restore_pane`
                // travelled with the spawn, so what lands is the *same* agent
                // pane, still running its conversation.
                && let Some(terminal) = view.terminal()
            {
                terminal.read(cx).run_command_line(&cmd);
            }
            Some(Pane::leaf(view))
        }
        SessionPane::Split { axis, ratio, a, b } => {
            let axis = match axis {
                SessionAxis::Horizontal => Axis::Horizontal,
                SessionAxis::Vertical => Axis::Vertical,
            };
            // One side failing collapses the split onto the survivor, exactly
            // as closing that pane by hand would.
            match (
                session_to_pane(workspace, a, alive, font_size, window, cx),
                session_to_pane(workspace, b, alive, font_size, window, cx),
            ) {
                (Some(a), Some(b)) => Some(Pane::split_node(axis, *ratio, a, b)),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    }
}

/// Build a shell-backed terminal view, wiring the per-pane subscriptions every
/// pane needs. Fallible: the daemon can refuse the spawn (it died, it's
/// wedged, the shell doesn't exist), and every caller here runs inside a gpui
/// input callback, where a panic can't unwind and would abort the app instead
/// of surfacing the failure. Report it, don't `expect` it.
///
/// `workspace` is **the switch that makes a window remote**: it picks the route
/// the pane's daemon connection takes and, through `ShellParts`, binds the view
/// to the same machine so everything pane-addressed afterwards (`Kill`, the
/// restore `List`, a reconnect's `Attach`) goes back to it. `None` is a local
/// pane, byte-for-byte what it always was.
fn new_terminal(
    workspace: Option<crate::terminal::PaneWorkspace>,
    font_size: f32,
    working_directory: Option<std::path::PathBuf>,
    restore_pane: Option<u64>,
    shell: Option<ShellSpec>,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> anyhow::Result<PaneSlot> {
    // The fork this whole `PaneSlot` business exists for. `PaneRoute` is the
    // same value that decides whether the connection carries a route header at
    // all, so "does this pane talk to another computer" is asked once, here.
    //
    // A local pane keeps the synchronous path — not for lack of generality, but
    // because it is ready in well under a millisecond and routing it through a
    // placeholder would paint one frame of a spinner on every ⌘T. See
    // `ui::pending_pane` for the whole argument.
    if matches!(
        crate::terminal::PaneRoute::for_workspace(workspace.as_ref()),
        crate::terminal::PaneRoute::Local
    ) {
        let parts = TerminalView::spawn_shell_terminal_in(
            workspace,
            working_directory,
            restore_pane,
            shell,
        )?;
        return Ok(PaneSlot::Ready(build_terminal_view(
            parts, font_size, window, cx,
        )));
    }

    // Everything else waits on another machine, so it waits *in the tree*.
    let spawn = crate::ui::pending_pane::PendingSpawn {
        workspace,
        working_directory,
        restore_pane,
        shell,
        font_size,
    };
    // The machine as the user knows it. `RemoteTarget`'s `Display` is the same
    // label the switcher's rows carry, so "Connecting to gpu-01…" names the row
    // that was clicked.
    let machine = spawn
        .workspace
        .as_ref()
        .map(|w| w.target.to_string())
        .unwrap_or_else(|| "the daemon".to_string());
    let pending = cx.new(|cx| crate::ui::pending_pane::PendingPane::new(machine, spawn, cx));
    cx.subscribe_in(
        &pending,
        window,
        |_app, pending, _: &crate::ui::pending_pane::RetryRequested, window, cx| {
            start_pane_spawn(pending.clone(), window, cx);
        },
    )
    .detach();
    start_pane_spawn(pending.clone(), window, cx);
    Ok(PaneSlot::Connecting(pending))
}

/// Run (or re-run) the blocking half of a pending pane's spawn, off the UI
/// thread, and land the result back in the tree.
fn start_pane_spawn(
    pending: Entity<crate::ui::pending_pane::PendingPane>,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) {
    let spawn = pending.read(cx).spawn.clone();
    let slot_id = pending.entity_id();
    let font_size = spawn.font_size;
    cx.spawn_in(window, async move |this, cx| {
        let parts = cx
            .background_executor()
            .spawn(async move {
                let attempt = |restore| {
                    TerminalView::spawn_shell_terminal_in(
                        spawn.workspace.clone(),
                        spawn.working_directory.clone(),
                        restore,
                        spawn.shell.clone(),
                    )
                };
                match spawn.restore_pane {
                    // Restore, on a machine nobody asked "which panes are still
                    // alive?" — because asking is itself a routed round trip and
                    // the UI thread is where that question used to be asked from
                    // (`alive_panes_on`). Trying the attach *is* the question,
                    // and a failed one costs one round trip on a connection this
                    // pane needed open anyway.
                    //
                    // Falling back to a fresh pane rather than surfacing the
                    // error: an id that is gone is the ordinary case after the
                    // remote's daemon has restarted, and a pane the user cannot
                    // get back is not worth a slot that only offers "Try Again".
                    Some(id) => attempt(Some(id)).or_else(|e| {
                        log::info!("pane {id} is gone on its machine ({e:#}); spawning fresh");
                        attempt(None)
                    }),
                    None => attempt(None),
                }
                // Flattened to a string here rather than carried as an
                // `anyhow::Error`: the chain is not `Send` across this await in
                // a form worth keeping, and what the pane shows is the rendered
                // message anyway.
                .map_err(|e| format!("{e:#}"))
            })
            .await;
        let _ = this.update_in(cx, |app, window, cx| {
            app.land_pane(slot_id, &pending, parts, font_size, window, cx);
        });
    })
    .detach();
}

/// Wire the per-pane subscriptions every pane needs around a freshly built
/// terminal. Shared by the synchronous local path and the async remote one, so
/// a pane that arrived late is wired identically to one that was there from the
/// first frame.
fn build_terminal_view(
    parts: crate::terminal::view::ShellParts,
    font_size: f32,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> Entity<TerminalView> {
    let view = cx.new(|cx| {
        let mut view = TerminalView::from_shell_parts(parts, window, cx);
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
    watch_pane_focus(&view, window, cx);
    view
}

/// Kill a daemon pane without blocking the window.
///
/// `kill_pane_on` opens a connection down the pane's route and writes one
/// frame — which against a *remote* route means the daemon opening an SSH
/// channel first, so doing it inline froze the window every time a remote pane
/// or tab was closed. Fire-and-forget by nature (a missing daemon means there
/// is nothing to kill anyway), so there is nothing to wait for and nothing to
/// report.
fn kill_pane_off_thread(route: crate::terminal::PaneRoute, pane_id: u64, cx: &mut App) {
    cx.background_executor()
        .spawn(async move { crate::terminal::RemoteTerminal::kill_pane_on(&route, pane_id) })
        .detach();
}

/// Re-render the app whenever `view` takes focus. Nothing else does this: a
/// pane owns its own focus handle, so clicking between splits notifies the
/// *pane*, not us, and any chrome that describes "the active pane" — the right
/// panel's Info and Changes tabs — would keep showing the pane you left until
/// some unrelated notify happened to repaint. Focus changes are user-paced, so
/// the extra frames are free.
fn watch_pane_focus(view: &Entity<TerminalView>, window: &mut Window, cx: &mut Context<Tty7App>) {
    let handle = view.read(cx).focus_handle.clone();
    let app = cx.weak_entity();
    window
        .on_focus_in(&handle, cx, move |_window, cx| {
            if let Some(app) = app.upgrade() {
                app.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();
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
    watch_pane_focus(&view, window, cx);
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
    use super::{
        TabAgentSession, leaf_shares_the_window_daemon, parse_ssh_connect_input,
        parse_ssh_option_words,
    };

    /// A remote window's saved layout can hold a native-SSH pane, whose russh
    /// session runs in *this* client's daemon rather than the machine's. Its
    /// saved id must not be matched against the remote's pane list: the two
    /// daemons number panes independently, so `1` over there is a different
    /// pane, and restoring it would swap the user's SSH tab for whatever the
    /// remote happens to be running.
    #[test]
    fn a_native_ssh_leaf_in_a_remote_window_is_not_looked_up_in_the_remote_daemon() {
        assert!(!leaf_shares_the_window_daemon(true, true));
        // Everything else is the window's own daemon: a shell in a remote
        // window is a pane over there, and in a local window both kinds are
        // panes here.
        assert!(leaf_shares_the_window_daemon(true, false));
        assert!(leaf_shares_the_window_daemon(false, true));
        assert!(leaf_shares_the_window_daemon(false, false));
    }

    // The single gate every fork surface consults. All three conditions have to
    // hold: an agent with a verified fork command, a session id the hooks have
    // reported, and a local pane — a remote one would shell the *local* agent
    // and branch the wrong machine's session.
    #[test]
    fn a_fork_needs_a_command_an_id_and_a_local_pane() {
        let session = |fork_label, session_id: Option<&str>, remote| TabAgentSession {
            fork_label,
            session_id: session_id.map(str::to_string),
            remote,
        };
        assert!(session(Some("Fork Session"), Some("abc"), false).forkable());
        assert!(
            !session(None, Some("abc"), false).forkable(),
            "an agent with no fork command is never forkable"
        );
        assert!(
            !session(Some("Fork Session"), None, false).forkable(),
            "no session id yet — the hooks haven't reported one"
        );
        assert!(
            !session(Some("Fork Session"), Some("abc"), true).forkable(),
            "a remote pane would fork the wrong machine's session"
        );
    }

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
        // Every keybinding edit below goes through `update_config`, which ends in
        // `Config::save()` — a *full* overwrite of `config.json` at whatever path
        // the config dir resolves to. Unpinned, that is the developer's real
        // `~/.config/tty7/config.json`, so running these tests silently reset the
        // user's entire config to `Config::default()` plus the shortcut recorded
        // here. The test-only `Config::save` now panics rather than allow that;
        // pin a scratch dir so it doesn't have to.
        crate::core::config::pin_test_config_dir();

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
            let app =
                cx.new(|cx| Tty7App::with_session(None, Some(Session::default()), window, cx));
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

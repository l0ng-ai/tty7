use gpui::{
    App, Axis, Bounds, Context, Edges, Entity, Focusable, Pixels, PromptLevel, Size, Subscription,
    Window, div, img, point, prelude::*, px, size,
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
    Config, CursorStyle as ConfigCursorStyle, MouseZoomModifier, NewTabPosition, RightPanelTab,
    ShellConfig, TabBarPosition, WindowBackdrop,
};
use crate::core::session::{
    Session, SessionAxis, SessionPane, SessionTab, WorkspaceId, WorkspaceStore,
};
use crate::core::shells::ShellInventory;
use crate::core::ssh_config;
use crate::core::window_state::{WindowGeometry as _, WindowState};
use crate::daemon::protocol::{RemoteContext, ShellSpec, ssh_option_takes_value};
use crate::daemon::spawn::DaemonMismatch;
use crate::terminal::view::{ChildExited, TerminalView};
use crate::ui::forwards::{ForwardFields, added_forward, rule_of};
use crate::ui::host_registry::HostId;
use crate::ui::i18n::{L10nKey, set_locale, t, t_fmt, t_plural};
use crate::ui::palette::{
    ChromeState, Command, CommandGroup, CommandKind, PaletteEvent, PaletteView,
};
use crate::ui::pane::{CloseOutcome, Dir, Pane, PaneSlot};
use crate::ui::presets::Fill;
use crate::ui::scm::ScmIntent;
use crate::ui::settings::{Recording, SettingsSection, SettingsState, ThemeEditor};
use crate::ui::theme::{apply_theme, set_menus};

/// What to start in a pane that is about to be opened.
pub(crate) enum SpawnAs {
    /// A local shell; `None` is whatever the default one is.
    Shell(Option<ShellSpec>),
    Ssh(Box<crate::daemon::protocol::NativeSshSpec>),
}

/// Where a row taken out of the new-tab menu lands.
///
/// Windows Terminal's rule, and the reason this is a parameter rather than two
/// menus: the same list of shells and hosts serves both, and ⌥ at click time
/// picks between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpawnWhere {
    NewTab,
    /// Beside the pane in front of the user, on [`Axis::Horizontal`] — the
    /// side `SplitRight` already puts a pane on. Deliberately not Windows
    /// Terminal's aspect-ratio guess: a split that lands somewhere different
    /// depending on the shape of the pane cannot be aimed.
    Split,
}

impl SpawnWhere {
    /// Read off the live keyboard state at click time. The menu hands its
    /// handler a synthetic click with no modifiers on it, so there is nowhere
    /// else the ⌥ the user is holding can be found.
    pub(crate) fn from_modifiers(mods: gpui::Modifiers) -> Self {
        if mods.alt {
            SpawnWhere::Split
        } else {
            SpawnWhere::NewTab
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemeEdit {
    Background,
    Foreground,
    Accent,
    Cursor,
    Selection,
    Ansi(usize),
}

/// The 16 ANSI slots in the order a terminal numbers them: 0-7, then their
/// bright twins. A theme author needs to know that slot 9 is what `\e[91m`
/// paints — "Color 9" does not say that, and "Bright red" does.
const ANSI_COLOR_LABELS: [L10nKey; 16] = [
    L10nKey::AppThemeAnsiBlack,
    L10nKey::AppThemeAnsiRed,
    L10nKey::AppThemeAnsiGreen,
    L10nKey::AppThemeAnsiYellow,
    L10nKey::AppThemeAnsiBlue,
    L10nKey::AppThemeAnsiMagenta,
    L10nKey::AppThemeAnsiCyan,
    L10nKey::AppThemeAnsiWhite,
    L10nKey::AppThemeAnsiBrightBlack,
    L10nKey::AppThemeAnsiBrightRed,
    L10nKey::AppThemeAnsiBrightGreen,
    L10nKey::AppThemeAnsiBrightYellow,
    L10nKey::AppThemeAnsiBrightBlue,
    L10nKey::AppThemeAnsiBrightMagenta,
    L10nKey::AppThemeAnsiBrightCyan,
    L10nKey::AppThemeAnsiBrightWhite,
];

/// Looked up at render time, never stored: the editor outlives a language
/// change, and a label cached when it opened would stay in the old language.
pub(crate) fn theme_edit_label(edit: ThemeEdit) -> &'static str {
    t(match edit {
        ThemeEdit::Background => L10nKey::AppThemeColorBackground,
        ThemeEdit::Foreground => L10nKey::AppThemeColorForeground,
        ThemeEdit::Accent => L10nKey::AppThemeColorAccent,
        ThemeEdit::Cursor => L10nKey::AppThemeColorCursor,
        ThemeEdit::Selection => L10nKey::AppThemeColorSelection,
        ThemeEdit::Ansi(i) => ANSI_COLOR_LABELS[i.min(15)],
    })
}

fn hsla_to_u32(color: gpui::Hsla) -> u32 {
    let rgba: gpui::Rgba = color.into();
    let to = |f: f32| (f.clamp(0.0, 1.0) * 255.0).round() as u32;
    (to(rgba.r) << 16) | (to(rgba.g) << 8) | to(rgba.b)
}

// The steppers clamp to the same range `sanitize` allows, defined in
// tty7-core next to the validation: a local, narrower pair used to push a
// config-legal value the wrong way — `font_size: 50` shrank to 48 on "+",
// and the result was written back to the file (#550).
pub(crate) use crate::core::config::{
    FONT_SIZE_MAX, FONT_SIZE_MIN, LINE_HEIGHT_MAX, LINE_HEIGHT_MIN,
};

pub(crate) const FONT_SIZE_STEP: f32 = 1.0;

pub(crate) const UI_FONT_SIZE_STEP: f32 = 1.0;

pub(crate) const LINE_HEIGHT_STEP: f32 = 0.05;

const MAX_CLOSED_TABS: usize = 20;

const RESIZE_STEP: f32 = 0.05;

pub(crate) const RECORD_COMMIT_DELAY_MS: u64 = 650;

/// The narrowest the terminal column may be squeezed to by the panels beside
/// it: roughly forty columns at the default font, which is a prompt with room
/// to read what it printed.
pub(crate) const TERMINAL_MIN_W: f32 = 360.;

/// Half the period of the home page's cursor, near enough the terminal's own
/// 530ms that the two do not read as different clocks.
pub(crate) const HOME_CURSOR_BLINK_MS: u64 = 600;

/// How wide one side panel may grow, given the floor under the terminal and the
/// floor under the *other* panel.
///
/// Each panel used to cap itself at half the window and know nothing about the
/// other, so the two of them could take the whole of it between them — two
/// halves leave nothing. On a 720-point window with both open the terminal was
/// left about 260 points, twenty-odd columns of the one pane the window exists
/// to show, and dragging either panel wider took the difference out of it.
/// Reserving the terminal's floor and the other panel's floor first means the
/// pane the user came for is spoken for before either panel is, and the cap
/// that comes out of it bounds the drag and the layout alike — so a panel
/// dragged to its limit stays where it was dropped instead of springing back.
///
/// The half-window cap stays, and this takes whichever of the two binds harder.
/// It is the one that binds on a wide window, where the reservation alone would
/// have *raised* the ceiling — on a 1440-point window it works out to 864, so
/// dropping it would have let a panel grow past where it could before under
/// cover of a change that is only meant to take width away from panels.
///
/// `others_floor` is the sum of the floors under every *other* column that is
/// open — the panel opposite, and the document column when a file or a diff is
/// docked. It is zero for each of them that is closed, which is why this takes
/// floors rather than reading them: only the caller knows what is up.
pub(crate) fn side_panel_max(viewport: f32, own_floor: f32, others_floor: f32) -> f32 {
    (viewport - TERMINAL_MIN_W - others_floor)
        .min(viewport * SIDE_PANEL_MAX_RATIO)
        .max(own_floor)
}

/// The half of the window neither panel may grow past on its own.
const SIDE_PANEL_MAX_RATIO: f32 = 0.5;

/// The narrowest a docked document column may be squeezed to: the header, a
/// readable run of about thirty columns, and the status bar under them. Below
/// this a file is a ribbon of hyphenated fragments and the column is worth
/// less than the terminal width it costs.
pub(crate) const DOCUMENT_MIN_W: f32 = 280.;

/// How wide the docked document column is, given the width the terminal and the
/// document share — the window less the sidebar and the right panel — and the
/// share of it the user asked for.
///
/// `None` is the narrow-window answer: there is no way to give both the
/// terminal and a document a width worth reading, so the caller falls back to
/// filling the workspace for this frame. That fallback is *derived*, never
/// stored — widening the window docks again on the next frame, and the user's
/// saved `document_layout` is untouched throughout.
///
/// The named two-thirds deliberately runs past the half-window cap the side
/// panels obey. That cap is there so neither *panel* can dominate a wide
/// display; the document is the thing the user is reading, and an increment
/// that silently became a half on every window wider than about 720 points of
/// body would be a lie. The terminal's floor still binds.
pub(crate) fn document_column_px(body: f32, ratio: f32) -> Option<f32> {
    if !body.is_finite() || body < TERMINAL_MIN_W + DOCUMENT_MIN_W {
        return None;
    }
    let ratio = if ratio.is_finite() { ratio } else { 0.5 };
    Some((body * ratio).clamp(DOCUMENT_MIN_W, body - TERMINAL_MIN_W))
}

pub(crate) const TITLE_BAR_HEIGHT: f32 = 40.;

pub(crate) const TILE_SIZE: f32 = 32.;
pub(crate) const TILE_GLYPH: f32 = 13.;
/// A tile that sits in a body row rather than in chrome: the box shrinks to
/// the minimum hit target, but the glyph keeps the chrome size. An 11px glyph
/// here read as a disabled ornament next to 14px text, and put a second,
/// smaller folder in the same column as the panel's folder tab.
pub(crate) const TILE_SIZE_SM: f32 = 24.;
pub(crate) const TILE_GLYPH_SM: f32 = TILE_GLYPH;

/// The tile that lives *inside* a list row rather than beside one, for the
/// buttons a row reveals on hover.
///
/// A box below [`TILE_SIZE_SM`] because of width: three `TILE_SIZE_SM` squares
/// would eat 72 of the 236px a file name has to live in, where three of these
/// eat 54.
pub(crate) const TILE_SIZE_XS: f32 = 18.;
pub(crate) const TILE_GLYPH_XS: f32 = 11.;

/// The compensation a glyph gets when its art does not fill the box the rest
/// of the set fills.
///
/// Every icon tty7 draws itself sits on the same optical bound — 3.4..20.6 of
/// a 24 viewBox, 19.3 units of ink once the round caps are counted. Stock
/// lucide `close` is a bare 6..18 cross, 14 units, so at [`TILE_GLYPH`] it
/// carries a quarter less ink than the tiles beside it and reads as the one
/// disabled control in the row. 16 buys that back.
///
/// Reach for this only for art we do not own. `plus` used to be here for the
/// same reason and is not any more: it is ours, so it was redrawn onto the
/// bound instead of being scaled up at the call site — the fix that also
/// reaches the 24px tiles, which have no `_LINE` step to grow into.
///
/// Note what scaling a glyph up quietly buys along with the extent: 16/13 more
/// stroke. `plus` had been leaning on that, so moving it to [`TILE_GLYPH`]
/// thinned it by a fifth even though it got *longer*, and it had to take that
/// weight back in its own `stroke-width` — a cross is two hairlines with no
/// fill to hide behind, so it is the one glyph in the set drawn off the
/// family's weight, by exactly the 16/13 it lost and no more. `ui::assets`'
/// test carries that arithmetic. Anything else that leaves here owes the same
/// accounting, in both directions: an asset redrawn for the tiles that scaled
/// it up is still drawn beside the ones that never did.
pub(crate) const TILE_GLYPH_LINE: f32 = 16.;

pub(crate) const TILE_PAD: f32 = (TILE_SIZE - TILE_GLYPH) / 2.;
pub(crate) const TILE_PAD_SM: f32 = (TILE_SIZE_SM - TILE_GLYPH_SM) / 2.;

const DOCS_URL: &str = "https://github.com/l0ng-ai/tty7#readme";
const DISCORD_URL: &str = "https://discord.gg/s3dethqz2V";
const ISSUES_URL: &str = "https://github.com/l0ng-ai/tty7/issues/new";

pub(crate) const CONTENT_INSET: f32 = 12.;

const TILE_EDGE_GAP: f32 = 5.;

pub(crate) fn tile_trailing_inset() -> f32 {
    (CONTENT_INSET - TILE_PAD).max(TILE_EDGE_GAP)
}

pub(crate) fn tile_trailing_inset_sm() -> f32 {
    (CONTENT_INSET - TILE_PAD_SM).max(TILE_EDGE_GAP)
}

pub(crate) const TITLE_BAR_LEAD: f32 = if cfg!(target_os = "macos") { 80. } else { 12. };

pub(crate) const WINDOW_CONTROLS_W: f32 = if cfg!(target_os = "macos") { 0. } else { 102. };

pub(crate) fn title_bar_hug_offset() -> f32 {
    if cfg!(target_os = "macos") {
        0.
    } else {
        tile_trailing_inset() - TITLE_BAR_LEAD
    }
}

/// The bounds to remember for reopening this window at the same place.
///
/// Under client-side decorations the window's *outer* rectangle is the whole
/// surface, shadow included, and both Linux backends put the shadow back on
/// their own once the window is up: Wayland's first sized configure adds the
/// inset to whatever geometry was asked for (`compute_outer_size`), and on X11
/// the inset travels as `_GTK_FRAME_EXTENTS`, which a window manager only
/// advertises — and gpui only turns CSD on for — when it honours those extents
/// by keeping the *visible* frame put. Save the outer rectangle either way and
/// it comes back twice the shadow larger every launch; that is the bug Zed
/// fixed in ca9cee85e1 ("linux: Fix non-maximized Zed windows growing larger
/// across sessions", #22301), whose own measurements were taken on X11.
///
/// So save the inner rectangle, exactly as Zed does, on every platform. It is
/// the visible frame, which is what the backends re-inflate back to, and it
/// costs nothing elsewhere: `inner_window_bounds` defaults to `window_bounds`
/// on the `PlatformWindow` trait, and neither the macOS nor the Windows backend
/// nor gpui's `TestWindow` overrides it. On X11 without a compositor the same
/// holds for a different reason — `window_decorations()` answers `Server`, the
/// window border never calls `set_client_inset`, and the insets stay zero.
fn window_bounds_to_remember(window: &Window) -> Bounds<Pixels> {
    window.inner_window_bounds().get_bounds()
}

/// The band the tab strip claims for a drop, in the window's outer coordinates.
///
/// The strip is the top of the *content*, and under client-side decorations the
/// content sits inside the frame padding while `viewport` still measures the
/// whole surface, shadow included — so the band starts at the padding and stops
/// at the far edge of the frame, one padding in from each side. Anything else
/// leaves the outermost chips outside the zone that is supposed to contain
/// them. `window_paddings` answers `Edges::all(0)` under server-side
/// decorations, which is every platform but Linux CSD, so off Linux this stays
/// the full-width rectangle from the window's corner it has always been.
fn strip_band(viewport: Size<Pixels>, pad: Edges<Pixels>) -> Bounds<Pixels> {
    Bounds {
        origin: point(pad.left, pad.top),
        size: size(
            (viewport.width - pad.left - pad.right).max(px(0.)),
            px(TITLE_BAR_HEIGHT),
        ),
    }
}

pub(crate) const WINDOW_MARK_SIZE: f32 = 20.;

pub(crate) fn title_bar_drag(
    row: gpui::Stateful<gpui::Div>,
    key: &'static str,
    window: &mut gpui::Window,
    cx: &mut gpui::App,
) -> gpui::Stateful<gpui::Div> {
    window_move_gesture(row, key, window, cx).on_double_click(|_, window, _| {
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

/// The armed flag behind [`window_move_gesture`]. A single bool, but *where* it
/// lives is the whole point — see there.
pub(crate) struct WindowMoveArm {
    should_move: bool,
}

pub(crate) fn window_move_gesture(
    row: gpui::Stateful<gpui::Div>,
    key: &'static str,
    window: &mut gpui::Window,
    cx: &mut gpui::App,
) -> gpui::Stateful<gpui::Div> {
    let arm = window.use_keyed_state(key, cx, |_, _| WindowMoveArm { should_move: false });
    row.window_control_area(gpui::WindowControlArea::Drag)
        .on_mouse_down(
            gpui::MouseButton::Left,
            window.listener_for(&arm, |arm, _: &gpui::MouseDownEvent, _, _| {
                arm.should_move = true;
            }),
        )
        .on_mouse_down_out(
            window.listener_for(&arm, |arm, _: &gpui::MouseDownEvent, _, _| {
                arm.should_move = false;
            }),
        )
        .on_mouse_up(
            gpui::MouseButton::Left,
            window.listener_for(&arm, |arm, _: &gpui::MouseUpEvent, _, _| {
                arm.should_move = false;
            }),
        )
        .on_mouse_up_out(
            gpui::MouseButton::Left,
            window.listener_for(&arm, |arm, _: &gpui::MouseUpEvent, _, _| {
                arm.should_move = false;
            }),
        )
        .on_mouse_move(
            window.listener_for(&arm, |arm, _: &gpui::MouseMoveEvent, window, _| {
                if arm.should_move {
                    arm.should_move = false;
                    window.start_window_move();
                }
            }),
        )
}

pub(crate) fn window_mark() -> Option<impl IntoElement> {
    if cfg!(target_os = "macos") {
        return None;
    }
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

pub struct Tab {
    pub pane: Pane,
    pub name: Option<String>,
    /// Entity id of the pane that last held focus in this tab. Recorded when we
    /// leave the tab (see [`Tty7App::remember_active_pane`]) and restored on
    /// return, so switching away and back keeps the active pane instead of
    /// jumping to the first leaf. `None` for a tab never left, or after its
    /// focused pane closed — both fall back to `first_leaf()`.
    last_focused: Option<gpui::EntityId>,
    /// The pane zoomed in this tab, stashed here by `activate` while another
    /// tab is on screen — zoom is a tab's view state, not the window's, so
    /// looking at another tab and coming back must not lose it (#599). `None`
    /// while the tab is active: then the zoom lives in `Tty7App::maximized`.
    pub(crate) zoomed: Option<Entity<TerminalView>>,
    pub(crate) diff_overlay: Option<crate::ui::diff_overlay::DiffOverlayState>,
    pub(crate) code: Option<Box<crate::ui::code_editor::TabCode>>,
    pub(crate) sidebar_group: std::cell::RefCell<Option<std::path::PathBuf>>,
    pub(crate) overlay_top: OverlayTop,
    /// Whether this tab's document fills the workspace or docks beside the
    /// terminal, once the tab has been told. `None` follows `document_layout`
    /// in the config, which is the default a fresh tab starts from and the
    /// last explicit choice anyone made.
    ///
    /// Per tab rather than per window because what you are doing differs per
    /// tab: reading a long file in one while an agent works in another wants
    /// the whole window here and half of it there, and a global switch made
    /// each of those flip the other.
    pub(crate) document_layout: Option<crate::core::config::DocumentLayout>,
    pub(crate) tree_id: std::cell::Cell<tty7_core::core::machine::TabId>,
    /// Monotonic stamp of when this tab was last activated, used to order the
    /// switcher's tab column most-recently-used first. Zero means never.
    pub(crate) last_used: std::cell::Cell<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum OverlayTop {
    #[default]
    Code,
    Diff,
}

impl Tab {
    pub(crate) fn new(pane: Pane) -> Self {
        Self {
            pane,
            name: None,
            last_focused: None,
            zoomed: None,
            diff_overlay: None,
            code: None,
            overlay_top: OverlayTop::default(),
            document_layout: None,
            sidebar_group: std::cell::RefCell::new(None),
            tree_id: std::cell::Cell::new(tty7_core::core::machine::TabId::new()),
            last_used: std::cell::Cell::new(0),
        }
    }

    pub(crate) fn from_tree(tree: &tty7_core::core::machine::Tab, pane: Pane) -> Self {
        Self {
            pane,
            name: tree.name.clone(),
            last_focused: None,
            zoomed: None,
            diff_overlay: None,
            code: None,
            overlay_top: OverlayTop::default(),
            document_layout: None,
            sidebar_group: std::cell::RefCell::new(
                tree.sidebar_group.clone().map(std::path::PathBuf::from),
            ),
            tree_id: std::cell::Cell::new(tree.id),
            last_used: std::cell::Cell::new(0),
        }
    }

    fn focus_target(&self) -> Option<crate::ui::pane::PaneSlot> {
        match self.last_focused {
            Some(id) => self.pane.leaf_matching_or_first(|l| l.entity_id() == id),
            None => self.pane.first_leaf(),
        }
    }

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

    /// The leaf a tab names itself after — its title and, with it, the home
    /// that title's path is measured against.
    fn title_leaf(&self, window: Option<&Window>, cx: &App) -> Option<Entity<TerminalView>> {
        let leaf = match window {
            Some(window) => self
                .pane
                .focused_leaf(window, cx)
                .or_else(|| self.focus_target()),
            None => self.focus_target(),
        };
        leaf.and_then(|l| l.terminal().cloned())
    }

    pub(crate) fn leaf_title(&self, window: Option<&Window>, cx: &App) -> String {
        self.title_leaf(window, cx)
            .map(|l| l.read(cx).title.clone())
            .unwrap_or_default()
    }

    /// [`Self::leaf_title`] together with what a `~` in it would mean — one
    /// leaf lookup, so the title and the home shortening it can never come
    /// from different panes (#580).
    pub(crate) fn leaf_title_and_home(
        &self,
        window: Option<&Window>,
        cx: &App,
    ) -> (String, Option<std::path::PathBuf>) {
        let Some(leaf) = self.title_leaf(window, cx) else {
            return (String::new(), None);
        };
        let leaf = leaf.read(cx);
        (leaf.title.clone(), leaf.display_home(cx))
    }

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

    pub(crate) fn agent(&self, cx: &App) -> Option<crate::core::cli_agent::CLIAgent> {
        self.pane
            .terminals()
            .into_iter()
            .find_map(|l| l.read(cx).agent())
    }

    /// The tab's most urgent agent leaf, named and reported by that one leaf.
    ///
    /// `agent` and `agent_status` answer independently — the *first* leaf
    /// carrying an agent, and the highest urgency found *anywhere* in the tab
    /// — so reading them as a pair can put one pane's name beside another
    /// pane's state, a row no leaf ever had. Anywhere both halves are shown
    /// at once reads them from here instead (#543).
    pub(crate) fn agent_row(
        &self,
        cx: &App,
    ) -> Option<(
        crate::core::cli_agent::CLIAgent,
        crate::core::cli_agent::AgentStatus,
    )> {
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
            .filter_map(|l| {
                let view = l.read(cx);
                let agent = view.agent()?;
                // A pane whose agent is running but has never reported a
                // session reads as idle, the same reading the badge has always
                // given it.
                let status = view
                    .agent_session()
                    .map(|s| s.status)
                    .unwrap_or(AgentStatus::Idle);
                Some((agent, status))
            })
            .max_by_key(|(_, status)| urgency(*status))
    }

    pub(crate) fn agent_status(&self, cx: &App) -> Option<crate::core::cli_agent::AgentStatus> {
        self.agent_row(cx).map(|(_, status)| status)
    }

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

pub(crate) struct Renaming {
    /// The tab being renamed, by tree id rather than index: an index drifts
    /// the moment any other tab closes or the strip reorders, which used to
    /// force every unrelated tab event to throw the half-typed name away —
    /// and left a window where the commit landed on the wrong tab (#598).
    pub(crate) tab: tty7_core::core::machine::TabId,
    pub(crate) input: Entity<InputState>,
    _subs: Vec<Subscription>,
}

pub(crate) struct WorkspaceRename {
    pub(crate) input: Entity<InputState>,
    _subs: Vec<Subscription>,
}

pub(crate) struct LoopbackForwardPanelState {
    pub(crate) form_pane_id: Option<u64>,
    pub(crate) managed: Vec<crate::daemon::protocol::ManagedForward>,
    pub(crate) mf_kind: crate::daemon::protocol::SshForwardKind,
    pub(crate) mf_bind_host: Entity<InputState>,
    pub(crate) mf_bind_port: Entity<InputState>,
    pub(crate) mf_target_host: Entity<InputState>,
    pub(crate) mf_target_port: Entity<InputState>,
    pub(crate) mf_description: Entity<InputState>,
    /// The rule the form is editing, whole rather than by id: an edit that
    /// fails has to be able to put back what it took out, and the id alone
    /// cannot describe the rule it named.
    pub(crate) mf_editing: Option<crate::daemon::protocol::ManagedForward>,
    /// Why the last Add or Save did not take, in the far side's own words.
    /// Cleared the moment the form is closed or the edit is abandoned.
    pub(crate) mf_error: Option<String>,
}

pub struct Tty7App {
    pub(crate) tabs: Vec<Tab>,
    pub(crate) active: usize,
    /// Hands out `Tab::last_used` stamps. A counter rather than a clock so two
    /// activations in the same second still order.
    tab_use_seq: std::cell::Cell<u64>,
    /// A tab asked for before its workspace finished hydrating.
    pending_tab: Option<tty7_core::core::machine::TabId>,
    pub(crate) font_size: f32,
    pub(crate) line_height: f32,
    pub(crate) font_family: String,
    pub(crate) font_family_bold: Option<String>,
    pub(crate) font_family_italic: Option<String>,
    pub(crate) font_features: Option<gpui::FontFeatures>,
    terminal_cursor_style: ConfigCursorStyle,
    terminal_scrollback_limit: usize,
    _config_watch: Subscription,
    _keystroke_watch: Subscription,
    _activation_watch: Subscription,
    _git_status_watch: Subscription,
    _pane_liveness_watch: Subscription,
    _appearance_watch: Subscription,
    palette: Option<Entity<PaletteView>>,
    palette_sub: Option<Subscription>,
    /// Preset that was live when the palette's theme picker started previewing.
    /// `Some` means the theme on screen is a preview that was never written to
    /// disk, and closing the palette without confirming puts this one back.
    theme_preview_restore: Option<String>,
    pub(crate) closed: Vec<SessionTab>,
    /// Set once the user has answered for the unwritten buffers in this
    /// window, so the close it then asks for goes straight through.
    ///
    /// `on_window_should_close` has to answer now and the prompt answers
    /// later, so the first close is refused and a second one is made from the
    /// prompt's landing. Without the flag that second close would ask again,
    /// and the window would never shut.
    close_confirmed: bool,
    pub(crate) renaming: Option<Renaming>,
    pub(crate) worktree_prompt: Option<crate::ui::worktree_prompt::WorktreePrompt>,
    pub(crate) maximized: Option<Entity<TerminalView>>,
    pub(crate) mod_hint_badges: bool,
    pub(crate) mod_hint_gen: u64,
    pub(crate) record_gen: u64,
    pub(crate) home_focus: gpui::FocusHandle,
    /// Whether the home page's cursor block is on this half-second.
    ///
    /// A `bool` a timer flips, not a per-frame animation — the same shape the
    /// terminal's own cursor uses. `with_animation(...).repeat()` asks for a
    /// frame sixty times a second to change one glyph's opacity twice, and the
    /// home page is otherwise perfectly still: it cost more than a live
    /// terminal did, on a window with nothing open in it.
    pub(crate) home_cursor_on: bool,
    pub(crate) shells: ShellInventory,
    pub(crate) shells_host: HostId,
    pub(crate) loopback_panel: LoopbackForwardPanelState,
    pub(crate) sftp_panel: crate::ui::sftp::SftpPanelState,
    pub(crate) right_panel: crate::ui::right_panel::RightPanelState,
    pub(crate) scm: crate::ui::scm::ScmPanelState,
    pub(crate) diff_probes_inflight:
        std::collections::HashSet<(crate::ui::host_ops::HostId, std::path::PathBuf)>,
    pub(crate) diff_probes_restale:
        std::collections::HashSet<(crate::ui::host_ops::HostId, std::path::PathBuf)>,
    pub(crate) file_tree: crate::ui::file_tree::FileTreeState,
    pub(crate) editor: crate::ui::code_editor::EditorPanelState,
    pub(crate) sidebar_width: Rc<Cell<f32>>,
    pub(crate) sidebar_dragging: Rc<Cell<bool>>,
    /// How much width a settings row will actually get, measured once per
    /// render. `settings_row` is called from page builders that never see the
    /// window, and the answer differs per page — the SSH page spends a host
    /// list on top of the nav before the row gets anything.
    pub(crate) settings_row_width: Cell<f32>,
    /// The window width the settings chrome sized itself against, measured in
    /// the same pass. The pages that render their own chrome — the SSH host
    /// list, the theme panel — are as blind to the window as `settings_row` is.
    pub(crate) settings_viewport_w: Cell<f32>,
    /// Cleared at the top of every settings render, then set by the first row
    /// the live search matched, so exactly one row per page carries the anchor
    /// the page scrolls to.
    pub(crate) settings_hit_anchored: Cell<bool>,
    pub(crate) right_panel_width: Rc<Cell<f32>>,
    pub(crate) right_panel_dragging: Rc<Cell<bool>>,
    /// The docked document column's share of the terminal column, live. Held
    /// beside the config value rather than in it for the same reason the two
    /// panel widths are: a drag writes this cell on every mouse move and the
    /// config once, on mouse up.
    pub(crate) document_ratio: Rc<Cell<f32>>,
    pub(crate) document_dragging: Rc<Cell<bool>>,
    pub(crate) right_panel_visible: bool,
    pub(crate) right_panel_tab: RightPanelTab,
    pub(crate) sidebar_collapsed: bool,
    pub(crate) sidebar_scroll: gpui::ScrollHandle,
    pub(crate) reorder: Rc<RefCell<Option<crate::ui::reorder::Reorder>>>,
    /// The pane the pointer is over, so only that one offers its drag handle.
    pub(crate) pane_hover: Rc<Cell<Option<gpui::EntityId>>>,
    pub(crate) pane_drag: crate::ui::pane_drag::PaneDragState,
    /// Where a tab held over the layout would be grafted in, as the last
    /// painted frame read it. The same bargain the pane drag's landing keeps:
    /// offered only once the tree agrees it changes something, so releasing
    /// over a highlight always does what the highlight showed.
    pub(crate) tab_merge: Cell<
        Option<(
            tty7_core::core::machine::TabId,
            crate::ui::pane_drag::DropZone<gpui::EntityId>,
        )>,
    >,
    /// The pane a drag would put down as a tab of its own, and where among the
    /// tabs it would go. Read back a frame later by the drop, like the two
    /// landings above.
    pub(crate) pane_detach: Cell<Option<(gpui::EntityId, usize)>>,
    /// Where the strip drew each tab's chip and where the sidebar drew each
    /// tab's row, by tab — the geometry a pane dropped on either of them reads
    /// its new place out of.
    ///
    /// Written from paint, so what a frame reads is where things were on the
    /// frame before. That is exactly as good as it needs to be: neither the
    /// strip nor the sidebar moves while a drag is in flight. A tab with no
    /// rectangle was not drawn — scrolled out of a full strip, filtered out of
    /// the sidebar — and takes no part in the reading.
    pub(crate) strip_slots: Rc<RefCell<Vec<Bounds<Pixels>>>>,
    pub(crate) sidebar_slots: Rc<RefCell<Vec<Bounds<Pixels>>>>,
    /// Where the active tab's panes were last drawn, which is the frame of
    /// reference a drag's landing is worked out in.
    pub(crate) pane_area: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub(crate) sidebar_search: Entity<InputState>,
    pub(crate) file_search: Entity<InputState>,
    _sidebar_search_sub: Subscription,
    _file_search_sub: Subscription,
    settings: Option<SettingsState>,
    pub(crate) ssh_prompt: crate::ui::ssh_prompt::SshPromptState,
    /// A close question is on screen. It carries no target: the answer acts on
    /// the tab or pane captured when the question was raised, not on whatever
    /// the app happens to be pointing at by the time it is answered.
    close_prompt_open: bool,
    window_bounds: Bounds<Pixels>,
    pub(crate) workspace: WorkspaceId,
    pub(crate) workspace_rename: Option<WorkspaceRename>,
    window_title: std::cell::RefCell<String>,
    pub(crate) connect: Option<crate::ui::remote_workspace::ConnectFlow>,
    pub(crate) switcher: Option<crate::ui::switcher::Switcher>,
    pub(crate) host_snapshots: std::collections::HashMap<
        crate::ui::host_registry::HostId,
        crate::ui::switcher::HostSnapshot,
    >,
    /// Errors reported for a remote host that should be shown inside that host's
    /// switcher group instead of as a global modal or toast.
    pub(crate) remote_host_errors: std::collections::HashMap<String, String>,
    /// Parked switcher groups (#485) whose notice the user dismissed by key —
    /// the entries stay, only the "will not reconnect" block is hidden.
    pub(crate) parked_dismissed: std::collections::HashSet<String>,
    /// A create asked of a machine that was not connected yet; the connect
    /// finishing is what completes it (see `Tty7App::finish_connect`).
    pub(crate) pending_create: Option<crate::ui::switcher::PendingCreate>,
    /// A create whose connect was refused for the control dialect — the state
    /// the "update server" button answers. Set aside rather than dropped, so
    /// the update the user runs next still ends in the workspace they asked
    /// for. Held apart from `pending_create` on purpose: only a connect to the
    /// same machine may spend it, and giving the machine up (dismissing the
    /// refusal, disconnecting) discards it.
    pub(crate) parked_create: Option<crate::ui::switcher::PendingCreate>,
    /// Why the window opened with no terminal in it. Shown on the home screen,
    /// which is otherwise indistinguishable from having closed everything.
    pub(crate) startup_error: Option<gpui::SharedString>,
}

/// What a raised close question is about. Tabs are named by their id, not their
/// index: a tab that exits on its own while the question is on screen shifts
/// every index after it, and answering "Close" must not then end a bystander.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseTarget {
    Tab(tty7_core::core::machine::TabId),
    Pane,
}

/// Why closing needs a question first. Closing a tab is the highest-frequency
/// destructive key in any terminal, and the product's headline claim is that
/// shells outlive the app — so the one action that permanently ends one has to
/// name what it is about to end.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum CloseReason {
    LiveSsh,
    /// A file open in this tab's code panel has edits that were never written.
    /// Carries the file's name, because "a file" is not something a reader can
    /// act on. Unlike the other two this loss is not recoverable: a killed
    /// command can be run again and a dropped SSH link reconnected, but the
    /// text is simply gone.
    UnsavedEdits(String),
    Busy(crate::terminal::view::PaneBusy),
}

/// The question to put to the user before ending work that is still going on.
fn close_prompt(ends_the_tab: bool, reason: &CloseReason) -> (String, String) {
    use crate::terminal::view::PaneBusy;
    use crate::ui::i18n::L10nKey;
    match reason {
        CloseReason::LiveSsh => (
            t(L10nKey::CloseSshConnectionTitle).to_string(),
            t(L10nKey::CloseSshConnectionBody).to_string(),
        ),
        CloseReason::UnsavedEdits(name) => (
            t(L10nKey::CloseUnsavedEditsTitle).to_string(),
            t_fmt(L10nKey::CloseUnsavedEditsBody, &[("name", name)]),
        ),
        CloseReason::Busy(busy) => {
            let title = match ends_the_tab {
                true => t(L10nKey::CloseTabBusyTitle),
                false => t(L10nKey::ClosePaneBusyTitle),
            };
            let body = match busy {
                PaneBusy::Command(what) => t_fmt(L10nKey::CloseBusyCommandBody, &[("what", what)]),
                PaneBusy::Agent(name) => t_fmt(L10nKey::CloseBusyAgentBody, &[("agent", name)]),
            };
            (title.to_string(), body)
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ForkPlacement {
    NewTab,
    Split { axis: Axis, before: bool },
}

pub(crate) struct TabAgentSession {
    pub(crate) fork_label: Option<&'static str>,
    pub(crate) session_id: Option<String>,
    pub(crate) remote: bool,
}

impl TabAgentSession {
    pub(crate) fn forkable(&self) -> bool {
        self.fork_label.is_some() && self.session_id.is_some() && !self.remote
    }
}

/// Maps a backdrop onto the settings dropdown. The dropdown lists the
/// presets the current Windows build supports, plus the stored value even
/// when unsupported here (see `theme::backdrop_options`), so the label
/// always matches what the window actually resolves to.
#[cfg(target_os = "windows")]
fn window_backdrop_index(backdrop: WindowBackdrop) -> usize {
    crate::ui::theme::backdrop_options(backdrop)
        .iter()
        .position(|candidate| *candidate == backdrop)
        .unwrap_or(0)
}

#[cfg(target_os = "windows")]
fn window_backdrop_from_index(idx: usize, current: WindowBackdrop) -> WindowBackdrop {
    crate::ui::theme::backdrop_options(current)
        .get(idx)
        .copied()
        .unwrap_or(WindowBackdrop::Auto)
}

#[cfg(target_os = "windows")]
fn window_backdrop_label_key(backdrop: WindowBackdrop) -> L10nKey {
    match backdrop {
        WindowBackdrop::Auto => L10nKey::SettingsBackdropAuto,
        WindowBackdrop::Blur => L10nKey::SettingsBackdropBlur,
        WindowBackdrop::Mica => L10nKey::SettingsBackdropMica,
        WindowBackdrop::MicaAlt => L10nKey::SettingsBackdropMicaAlt,
        WindowBackdrop::Acrylic => L10nKey::SettingsBackdropAcrylic,
        WindowBackdrop::Off => L10nKey::SettingsBackdropOff,
    }
}

#[cfg(target_os = "windows")]
fn window_backdrop_labels(backdrop: WindowBackdrop) -> Vec<String> {
    crate::ui::theme::backdrop_options(backdrop)
        .iter()
        .map(|backdrop| t(window_backdrop_label_key(*backdrop)).to_string())
        .collect()
}

/// What a full-window overlay (settings, the opened file, the diff view)
/// paints between its own fill and its content.
///
/// Those overlays fill opaquely on purpose, so the OS backdrop cannot show
/// through their text — but that fill also sits on top of the background
/// image the workspace root paints, and would erase it for as long as an
/// overlay is open. So each one repaints the image, then the workspace's own
/// translucent fill over it. That second layer is what keeps the overlay
/// readable: it dims the image to exactly the strength it had when these
/// overlays were themselves translucent, before they were made opaque.
///
/// Empty when the theme has no image — then the opaque fill alone is already
/// what the overlay wants, and a second pass of the same paint buys nothing.
pub(crate) fn overlay_surface_layers(cx: &App) -> Vec<gpui::Div> {
    match window_background_image_layer(cx) {
        Some(image) => vec![
            image,
            div()
                .absolute()
                .inset_0()
                .bg(crate::ui::theme::workspace_background(cx)),
        ],
        None => Vec::new(),
    }
}

/// The theme's background image as a full-bleed layer.
pub(crate) fn window_background_image_layer(cx: &App) -> Option<gpui::Div> {
    let image = cx
        .try_global::<crate::ui::presets::ActiveBackground>()?
        .image
        .clone()?;
    Some(
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
}

/// Clears the window overrides that are effective on the current platform.
/// `backdrop_is_local` models whether the Windows-only backdrop participates
/// in this platform's rendering and therefore belongs to its reset operation.
fn clear_window_override_values(config: &mut Config, backdrop_is_local: bool) {
    config.window_opacity = None;
    config.window_blur = None;
    if backdrop_is_local {
        config.window_backdrop = WindowBackdrop::Auto;
    }
}

impl Tty7App {
    pub fn for_workspace(
        id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::for_workspace_at(id, None, window, cx)
    }

    pub fn for_workspace_at(
        id: Option<WorkspaceId>,
        mut initial_cwd: Option<std::path::PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let restore = cx.global::<Config>().restore_session;
        let known = id.is_some_and(|id| WorkspaceStore::all(cx).get(id).is_some());
        let workspace = WorkspaceStore::claim(cx, id);
        let is_remote = WorkspaceStore::all(cx)
            .get(workspace)
            .is_some_and(|w| w.is_remote());
        // Tabs that exist on the machine are shown whatever the restore
        // setting says: that setting decides whether a window comes back at
        // launch, not whether an open one shows what is really in it. The
        // `else` arm below saves this window's session, and saving an empty
        // one over a live tree would erase it.
        let on_machine = id.is_some_and(|id| crate::ui::machine_mirror::machine_holds_tabs(cx, id));
        let hydrate = on_machine || (known && (restore || is_remote));
        let session = hydrate.then(Session::default);
        // A window that is about to pull its layout cannot also open the folder
        // the launch asked for as its first terminal — the pull would find a
        // window that already has a tab, decline to adopt into it, and push
        // that one tab back as the whole workspace. So the folder travels with
        // the hydration and becomes a tab once the layout is up.
        let open_after_hydrate = hydrate.then(|| initial_cwd.take()).flatten();
        let app = Self::with_session_at(Some(workspace), session, initial_cwd, window, cx);
        if hydrate {
            match open_after_hydrate {
                Some(cwd) => {
                    crate::ui::tree_sync::hydrate_window_then_open(cx, workspace, cwd);
                }
                None => crate::ui::tree_sync::hydrate_window_from_tree(cx, workspace),
            }
        } else {
            if !is_remote {
                crate::ui::tree_sync::mark_window_informed(cx, workspace);
            }
            app.save_session(cx);
        }
        Self::prompt_daemon_version_mismatch(window, cx);
        crate::ui::remote_connect::register(cx);
        crate::ui::remote_connect::sweep_wsl(cx);
        Self::prompt_remote_daemon_mismatch(window, cx);
        app.reopen_remote_at_startup(cx);
        app
    }

    /// Put a version-mismatched local server to the user, with no third way out.
    ///
    /// Both handshakes compare their version for equality and hang up on
    /// anything else — the pane protocol in `daemon::spawn::ensure_running`, the
    /// control dialect in `host::server`'s hello. So a server whose number
    /// disagrees cannot be talked round, and carrying on beside it is not a
    /// degraded mode but a broken one: panes still spawn while every
    /// machine-tree call is refused, which is how a window comes to open with no
    /// tabs and save none of the ones you make. This used to be offered as "Keep
    /// Shells", and taking it was indistinguishable from the bug.
    ///
    /// Restart or quit, then. Quitting is the half that destroys nothing: the
    /// server and every shell under it keep running, which is what makes it a
    /// real answer for someone who would rather go install the matching build
    /// than lose a session mid-flight.
    fn prompt_daemon_version_mismatch(window: &mut Window, cx: &mut Context<Self>) {
        let Some(mismatch) = crate::daemon::spawn::take_mismatched_daemon() else {
            return;
        };
        let ours = crate::daemon::protocol::PROTOCOL_VERSION;
        let detail = match &mismatch {
            DaemonMismatch::Protocol(Some(v)) => t_fmt(
                L10nKey::AppRestartServerMismatchDetail,
                &[
                    ("build", &v.build.to_string()),
                    ("protocol", &v.protocol.to_string()),
                    ("ours", &ours.to_string()),
                ],
            ),
            DaemonMismatch::Protocol(None) => t(L10nKey::AppRestartServerOldDetail).to_string(),
            // The handshake only reports disagreement, not direction, and a
            // daemon left behind by a newer build is as much a mismatch as one
            // left behind by an older. Calling that one "older" would be the
            // same wrong guess the remote path stopped making.
            DaemonMismatch::Dialect(refusal) => t_fmt(
                if refusal.peer < refusal.ours {
                    L10nKey::AppRestartServerDialectDetail
                } else {
                    L10nKey::AppRestartServerDialectNewerDetail
                },
                &[
                    ("build", &refusal.peer_build),
                    ("dialect", &refusal.peer.to_string()),
                    ("ours", &refusal.ours.to_string()),
                ],
            ),
        };
        // The one prompt here that does not use `confirm_answers`, because
        // neither answer is "leave it alone" — the app cannot carry on beside a
        // server it cannot speak to. With nothing safe to give Escape, this
        // keeps Quit at index 0 where Return lands: it arrives unasked at
        // launch, the moment a stray Return is most likely, and quitting loses
        // no sessions while restarting the server ends every one of them.
        let answer = window.prompt(
            PromptLevel::Warning,
            t(L10nKey::AppRestartServerTitle),
            Some(&detail),
            &[t(L10nKey::CmdQuitTty7), t(L10nKey::RestartServer)],
            cx,
        );
        cx.spawn(async move |this, cx| match answer.await {
            Ok(1) => {
                let _ = this.update_in(cx, |this, _window, cx| this.restart_daemon_confirmed(cx));
            }
            Ok(_) => {
                cx.update(|cx| cx.quit());
            }
            // Dismissed without an answer: the window went away before the
            // question was settled. Arm it again so the next window asks, rather
            // than letting the state this prompt exists to prevent slip through
            // the gap.
            Err(_) => crate::daemon::spawn::note_daemon_mismatch(mismatch),
        })
        .detach();
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn with_session(
        workspace: Option<WorkspaceId>,
        session: Option<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_session_at(workspace, session, None, window, cx)
    }

    fn with_session_at(
        workspace: Option<WorkspaceId>,
        session: Option<Session>,
        initial_cwd: Option<std::path::PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace = workspace.unwrap_or_default();
        let pane_ws = crate::ui::remote_workspace::pane_workspace_for(cx, workspace);
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
        let mf_bind_host = cx.new(|cx| InputState::new(window, cx).default_value("127.0.0.1"));
        let mf_bind_port = cx.new(|cx| InputState::new(window, cx).placeholder("8080"));
        let mf_target_host = cx.new(|cx| InputState::new(window, cx).placeholder("127.0.0.1"));
        let mf_target_port = cx.new(|cx| InputState::new(window, cx).placeholder("80"));
        let mf_description = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t(L10nKey::AppPlaceholderDescription))
        });
        let sidebar_width = cx.global::<Config>().sidebar_width;
        let right_panel_width = cx.global::<Config>().right_panel_width;
        let document_ratio = cx.global::<Config>().document_ratio;
        let right_panel_visible = cx.global::<Config>().right_panel_visible;
        let right_panel_tab = cx.global::<Config>().right_panel_tab;
        let scm_graph_expanded = cx.global::<Config>().scm_graph_expanded;
        let sidebar_collapsed = cx.global::<Config>().sidebar_collapsed;
        let config_watch = cx.observe_global_in::<Config>(window, |this, window, cx| {
            this.reload_from_config(window, cx)
        });
        cx.default_global::<crate::terminal::git_status::GitStatusCache>();
        let git_status_watch =
            cx.observe_global::<crate::terminal::git_status::GitStatusCache>(|this, cx| {
                this.maybe_refresh_diff_overlay(cx);
                this.right_panel_refresh_changes(cx);
                cx.notify();
            });
        cx.default_global::<crate::terminal::pane_liveness::PaneLivenessCache>();
        let pane_liveness_watch = cx
            .observe_global::<crate::terminal::pane_liveness::PaneLivenessCache>(|_this, cx| {
                cx.notify();
            });
        let this = cx.weak_entity();
        let keystroke_watch = cx.intercept_keystrokes(move |_ev, _window, cx| {
            let _ = this.update(cx, |this, cx| this.dismiss_mod_hint(cx));
        });
        let activation_watch = cx.observe_window_activation(window, |this, window, cx| {
            this.dismiss_mod_hint(cx);
            this.set_link_modifier(false, cx);
            // A modifier released over another window never reports here, so a
            // Ctrl+Tab panel would hang waiting for a commit that cannot come.
            this.switcher_release_hold(cx);
            if window.is_window_active() {
                WorkspaceStore::focus(cx, this.workspace);
                this.refresh_git_status_all(cx);
            }
        });
        let this = cx.weak_entity();
        let appearance_watch = window.observe_window_appearance(move |window, cx| {
            crate::ui::theme::note_system_appearance(window, cx);
            if !cx.global::<Config>().theme_follow_system {
                return;
            }
            apply_theme(Some(window), cx);
            let _ = this.update(cx, |this, cx| {
                this.rebuild_theme_editor(window, cx);
                this.sync_window_opacity_slider(window, cx);
                cx.notify();
            });
        });
        apply_theme(Some(window), cx);
        set_menus(cx);
        let mut startup_error: Option<gpui::SharedString> = None;
        let (tabs, active) = match session {
            None => match new_terminal(
                pane_ws.clone(),
                Some(workspace),
                font_size,
                initial_cwd,
                None,
                None,
                window,
                cx,
            ) {
                Ok(first) => (vec![Tab::new(Pane::leaf(first))], 0),
                Err(e) => {
                    log::error!("first terminal failed to start: {e}");
                    // The home screen is what a user sees when they have closed
                    // everything, and it used to be what they saw when tty7
                    // could not open anything — silently, on the very first
                    // launch, with the cause only in a log file.
                    startup_error = Some(gpui::SharedString::from(t_fmt(
                        L10nKey::AppOpenTerminalFailed,
                        &[("error", &e.to_string())],
                    )));
                    (Vec::new(), 0)
                }
            },
            some => {
                let (tabs, active, dropped) =
                    tabs_from_session(pane_ws.as_ref(), workspace, some, font_size, window, cx);
                if dropped > 0 {
                    startup_error = Some(gpui::SharedString::from(t_plural(
                        L10nKey::AppTabsNotRestored,
                        dropped,
                        &[],
                    )));
                }
                (tabs, active)
            }
        };
        let sidebar_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t(crate::ui::i18n::L10nKey::SearchTabs))
        });
        let sidebar_search_sub =
            cx.subscribe_in(&sidebar_search, window, |_this, _i, ev, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            });
        let file_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t(crate::ui::i18n::L10nKey::SearchFiles))
        });
        let file_search_sub = cx.subscribe_in(&file_search, window, |_this, _i, ev, _w, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        });
        let mut app = Self {
            tabs,
            active,
            tab_use_seq: std::cell::Cell::new(0),
            pending_tab: None,
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
            theme_preview_restore: None,
            closed: Vec::new(),
            close_confirmed: false,
            renaming: None,
            worktree_prompt: None,
            maximized: None,
            mod_hint_badges: false,
            mod_hint_gen: 0,
            record_gen: 0,
            home_focus: cx.focus_handle(),
            home_cursor_on: true,
            shells: ShellInventory::default(),
            shells_host: HostId::LOCAL,
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
                mf_error: None,
            },
            sftp_panel,
            right_panel: Default::default(),
            scm: crate::ui::scm::ScmPanelState {
                graph: crate::ui::scm::GraphState {
                    expanded: scm_graph_expanded,
                    ..Default::default()
                },
                ..Default::default()
            },
            diff_probes_inflight: Default::default(),
            diff_probes_restale: Default::default(),
            file_tree,
            editor,
            sidebar_width: Rc::new(Cell::new(sidebar_width)),
            sidebar_dragging: Rc::new(Cell::new(false)),
            settings_row_width: Cell::new(f32::MAX),
            settings_viewport_w: Cell::new(f32::MAX),
            settings_hit_anchored: Cell::new(false),
            right_panel_width: Rc::new(Cell::new(right_panel_width)),
            right_panel_dragging: Rc::new(Cell::new(false)),
            document_ratio: Rc::new(Cell::new(document_ratio)),
            document_dragging: Rc::new(Cell::new(false)),
            right_panel_visible,
            right_panel_tab,
            sidebar_collapsed,
            sidebar_scroll: gpui::ScrollHandle::new(),
            reorder: Rc::new(RefCell::new(None)),
            pane_hover: Rc::new(Cell::new(None)),
            pane_drag: Rc::new(RefCell::new(None)),
            tab_merge: Cell::new(None),
            pane_detach: Cell::new(None),
            strip_slots: Rc::new(RefCell::new(Vec::new())),
            sidebar_slots: Rc::new(RefCell::new(Vec::new())),
            pane_area: Rc::new(Cell::new(None)),
            sidebar_search,
            _sidebar_search_sub: sidebar_search_sub,
            file_search,
            _file_search_sub: file_search_sub,
            settings: None,
            ssh_prompt: crate::ui::ssh_prompt::SshPromptState::new(cx),
            close_prompt_open: false,
            window_bounds: window_bounds_to_remember(window),
            workspace,
            workspace_rename: None,
            window_title: std::cell::RefCell::new(String::new()),
            connect: None,
            switcher: None,
            host_snapshots: std::collections::HashMap::new(),
            remote_host_errors: std::collections::HashMap::new(),
            parked_dismissed: std::collections::HashSet::new(),
            pending_create: None,
            parked_create: None,
            startup_error,
        };
        if !cfg!(test) && crate::ui::windows::WindowRegistry::count(cx) == 0 {
            crate::ui::tray::init(cx);
        }
        app.refresh_shells(cx);
        cx.on_app_quit(|app, cx| {
            app.save_session(cx);
            crate::core::window_state::WindowState::from_bounds(app.window_bounds).save();
            async move {}
        })
        .detach();

        cx.observe_window_bounds(window, |this, window, _cx| {
            this.window_bounds = window_bounds_to_remember(window);
        })
        .detach();

        // The home page's cursor, on the terminal's own schedule. It ticks
        // whether or not the page is up — a timer that wakes twice a second to
        // compare a `Vec`'s length against zero costs nothing — but only asks
        // for a frame when the page is the thing on screen.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(HOME_CURSOR_BLINK_MS))
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.tabs.is_empty() {
                            this.home_cursor_on = !this.home_cursor_on;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let weak_app = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            // Asked before anything is taken down. Closing a window keeps the
            // shells — they are the daemon's — but the code panel's buffers
            // are this window's alone and nothing writes them anywhere, so
            // this is the last moment they exist.
            if let Some(app) = weak_app.upgrade()
                && let Some((tab, name)) = app.read(cx).unsaved_edit_to_confirm()
            {
                app.update(cx, |app, cx| app.confirm_window_close(tab, name, window, cx));
                return false;
            }
            let last_window = crate::ui::windows::WindowRegistry::count(cx) <= 1;
            if let Some(app) = weak_app.upgrade() {
                app.update(cx, |app, cx| app.detach_workspace(cx));
            }
            if last_window {
                // With a tray icon, closing the last window retires to the
                // tray: the daemon stays reachable (show / quit-and-stop)
                // instead of being orphaned behind a dead icon. Without one
                // the app quits — the only way it stays visible at all.
                //
                // The icon has to actually be up, not merely asked for: the
                // backend can fail for the whole run (a Linux session with no
                // StatusNotifier host), and retiring into an icon that never
                // appeared leaves a process with no window and no tray — no
                // way back in, and the daemon still held.
                let retire_to_tray =
                    cx.global::<Config>().show_tray_icon && crate::ui::tray::icon_is_up();
                if !retire_to_tray {
                    cx.spawn(async move |cx| {
                        cx.update(|cx| cx.quit());
                    })
                    .detach();
                }
            }
            true
        });

        app.focus_active(window, cx);
        app
    }

    pub(crate) fn save_session(&self, cx: &mut App) {
        for view in self.tabs.iter().flat_map(|tab| tab.pane.terminals()) {
            let Some(owner) = view.read(cx).owner_workspace() else {
                continue;
            };
            if owner != self.workspace {
                log::error!(
                    "save_session: window of workspace {} is recording pane {} \
                     that was created for workspace {owner} — cross-workspace \
                     write detected, please report this",
                    self.workspace,
                    view.read(cx).pane_id,
                );
            }
        }
        WorkspaceStore::record_geometry(
            cx,
            self.workspace,
            WindowState::from_bounds(self.window_bounds),
        );
        crate::ui::tree_sync::sync_window(self, cx);
    }

    pub(crate) fn detach_workspace(&self, cx: &mut App) {
        self.save_session(cx);
        let answered = WorkspaceStore::machine_is_connected(cx, self.workspace);
        if self.tabs.is_empty()
            && answered
            && crate::ui::tree_sync::workspace_is_disposable(cx, self.workspace)
        {
            crate::ui::tree_sync::fire_workspace_op(cx, self.workspace, |ws| {
                tty7_core::daemon::control::ControlRequest::WorkspaceRemove { workspace: ws }
            });
            WorkspaceStore::remove(cx, self.workspace);
        } else {
            WorkspaceStore::close_window(cx, self.workspace);
        }
        crate::ui::windows::WindowRegistry::unregister(cx, self.workspace);
        crate::ui::tree_sync::forget(cx, self.workspace);
        crate::ui::windows::refresh_menu(cx);
    }

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
            log::warn!(
                "workspace {:?} is closing with no pane left to address it, so its \
                 forwards cannot be torn down; any it opened stay bound until the \
                 daemon exits",
                self.workspace
            );
            return;
        };
        cx.background_executor()
            .spawn(async move {
                match route.teardown() {
                    None => log::warn!(
                        "a workspace teardown could not be delivered; its forwards \
                         stay bound until the daemon exits"
                    ),
                    Some(left) if !left.is_empty() => {
                        log::warn!("{} forwards survived a workspace teardown", left.len())
                    }
                    Some(_) => {}
                }
            })
            .detach();
    }

    pub(crate) fn stop_workspace(
        &mut self,
        id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::ui::windows::confirm_and_stop(cx, window, id);
        cx.notify();
    }

    pub(crate) fn delete_workspace(
        &mut self,
        id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        crate::ui::windows::confirm_and_delete(cx, window, id);
        cx.notify();
    }

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
        // Switching workspaces happens in place. A second window is something
        // you ask for — with the platform modifier, or "Open in New Window".
        self.switch_workspace(Some(id), window, cx);
    }

    /// Trades this window's workspace for another one. `None` starts a fresh
    /// workspace here rather than opening one in a new window.
    pub(crate) fn switch_workspace(
        &mut self,
        id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous = self.workspace;
        if id == Some(previous) {
            return;
        }
        // Anything parked for the workspace we are leaving is now meaningless.
        self.pending_tab = None;
        if self.tabs.is_empty() && crate::ui::tree_sync::workspace_is_disposable(cx, previous) {
            crate::ui::tree_sync::fire_workspace_op(cx, previous, |ws| {
                tty7_core::daemon::control::ControlRequest::WorkspaceRemove { workspace: ws }
            });
            WorkspaceStore::remove(cx, previous);
        } else if self.tabs.is_empty() {
            WorkspaceStore::close_window(cx, previous);
        } else {
            self.save_session(cx);
            WorkspaceStore::close_window(cx, previous);
        }
        crate::ui::tree_sync::forget(cx, previous);

        let claimed = WorkspaceStore::claim(cx, id);
        crate::ui::windows::WindowRegistry::rebind(cx, previous, claimed);
        crate::ui::remote_workspace::RemoteLinks::supervise(cx, claimed);
        self.adopt_workspace(claimed, Session::default(), window, cx);
        // This method runs under the app's own update lease, so the tabs the
        // pull must see are the ones just adopted here — reading the app back
        // out of `cx` (as `tabs_on_screen` would) is an abort in gpui.
        let showing = self.tabs.iter().map(|t| t.tree_id.get()).collect();
        crate::ui::tree_sync::hydrate_window_with_tabs(cx, claimed, showing);
    }

    pub(crate) fn adopt_workspace(
        &mut self,
        id: WorkspaceId,
        session: Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_host = self.spawn_host(cx);
        self.workspace = id;
        self.rebind_host(previous_host, cx);
        self.refresh_shells(cx);
        let font_size = self.font_size;
        let pane_ws = self.window_workspace(cx);
        let (tabs, active, dropped) = tabs_from_session(
            pane_ws.as_ref(),
            self.workspace,
            Some(session),
            font_size,
            window,
            cx,
        );
        if dropped > 0 {
            window.push_notification(t_plural(L10nKey::AppTabsNotRestored, dropped, &[]), cx);
        }
        self.tabs = tabs;
        self.active = active;
        self.maximized = None;
        self.save_session(cx);
        crate::ui::windows::refresh_menu(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    fn reopen_closed_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(st) = self.closed.pop() else {
            return;
        };
        let pane_ws = self.window_workspace(cx);
        let alive = alive_panes_on(&crate::terminal::PaneRoute::for_workspace(pane_ws.as_ref()));
        let Some(pane) = session_to_pane(
            pane_ws.as_ref(),
            self.workspace,
            &st.pane,
            alive.as_ref(),
            self.font_size,
            window,
            cx,
        ) else {
            window.push_notification(t(L10nKey::AppReopenTabFailed), cx);
            self.closed.push(st);
            return;
        };
        self.remember_active_pane(window, cx);
        self.maximized = None;
        let insert_at = self.new_tab_insert_at(cx);
        self.tabs.insert(
            insert_at,
            Tab {
                pane,
                name: st.name,
                last_focused: None,
                zoomed: None,
                diff_overlay: None,
                code: None,
                overlay_top: OverlayTop::default(),
                document_layout: None,
                sidebar_group: std::cell::RefCell::new(st.sidebar_group),
                tree_id: std::cell::Cell::new(tty7_core::core::machine::TabId::new()),
                last_used: std::cell::Cell::new(0),
            },
        );
        self.active = insert_at;
        self.focus_active(window, cx);
        self.save_session(cx);
        cx.notify();
    }

    pub(crate) fn owns_leaf(&self, leaf_id: u64) -> bool {
        self.tabs.iter().any(|t| {
            t.pane
                .leaves()
                .iter()
                .any(|l| l.entity_id().as_u64() == leaf_id)
        })
    }

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

    pub(crate) fn handle_tray_action(
        &mut self,
        action: crate::ui::tray::TrayAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::ui::tray::TrayAction;
        fn surface_window(window: &mut Window, cx: &mut App) {
            cx.activate(true);
            window.activate_window();
        }
        match action {
            TrayAction::ShowWindow => surface_window(window, cx),
            TrayAction::RevealPane { leaf_id } => {
                let tab_ix = self.tabs.iter().position(|t| {
                    t.pane
                        .leaves()
                        .iter()
                        .any(|l| l.entity_id().as_u64() == leaf_id)
                });
                if let Some(ix) = tab_ix {
                    self.activate(ix, window, cx);
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
                self.check_for_updates_now(window, cx);
            }
            // One exit path with one meaning: quit the app and stop the server
            // (after the confirmation that protects running shells). Closing
            // the window is the keep-everything exit — it retires to the tray.
            TrayAction::Quit => self.quit_stop_sessions(window, cx),
        }
    }

    fn quit_stop_sessions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.activate(true);
        window.activate_window();
        let answer = window.prompt(
            PromptLevel::Warning,
            t(crate::ui::i18n::L10nKey::QuitStopServerTitle),
            Some(t(crate::ui::i18n::L10nKey::QuitStopServerBody)),
            &crate::ui::confirm_answers(
                t(crate::ui::i18n::L10nKey::QuitAndStop),
                t(crate::ui::i18n::L10nKey::Cancel),
            ),
            cx,
        );
        cx.spawn(async move |_this, cx| {
            if !matches!(answer.await, Ok(0)) {
                return;
            }
            cx.background_spawn(async { crate::daemon::spawn::stop() })
                .await;
            cx.update(|cx| cx.quit());
        })
        .detach();
    }

    pub(crate) fn restart_window_daemon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(remote) = WorkspaceStore::remote_ref(cx, self.workspace) else {
            self.restart_daemon(window, cx);
            return;
        };
        let target = remote.target.clone();
        let label = crate::ui::remote_connect::route_label(cx, &remote);
        if !target.hosts_our_server() {
            window.push_notification(
                t_fmt(L10nKey::AppRestartServerNoServer, &[("label", &label)]),
                cx,
            );
            return;
        }
        // A server the other side of a dialect bump has nothing to restart
        // *into*: the binary this build launches was never installed over
        // there, and the installer now refuses rather than killing the one
        // that is running. Updating is the move that works, so offer that,
        // under its own name and with its own warning about ending sessions.
        if let Some(crate::ui::remote_workspace::RemoteStatus::ServerMismatch(refusal)) =
            self.remote_status(cx)
        {
            let action = crate::ui::remote_workspace::mismatch_action_key(&refusal);
            self.confirm_replace_remote_server(target, label, action, window, cx);
            return;
        }
        self.confirm_restart_remote_server(target, label, window, cx);
    }

    pub(crate) fn restart_daemon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Two different actions wearing one name. Where the service can rewrite
        // itself in place, nothing in a pane is interrupted and promising the
        // user a bloodbath would be a lie that costs them the feature; where it
        // cannot, every running command really does end, and that is the one
        // thing they need to be told before they agree.
        let in_place =
            crate::daemon::spawn::local_daemon_supports(crate::daemon::protocol::FEATURE_HANDOFF);
        let answer = window.prompt(
            PromptLevel::Warning,
            t(L10nKey::AppRestartServerTitle),
            Some(t(if in_place {
                L10nKey::AppRestartServerBodyInPlace
            } else {
                L10nKey::AppRestartServerBody
            })),
            &crate::ui::confirm_answers(
                t(L10nKey::AppRestart),
                t(crate::ui::i18n::L10nKey::Cancel),
            ),
            cx,
        );
        cx.spawn(async move |this, cx| {
            if !matches!(answer.await, Ok(0)) {
                return;
            }
            let _ = this.update_in(cx, |this, _window, cx| this.restart_daemon_confirmed(cx));
        })
        .detach();
    }

    fn restart_daemon_confirmed(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
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
            let restarted = cx
                .background_spawn(async move {
                    // The same test that chose the dialog's copy chooses the
                    // action, because the copy is a promise. A daemon that
                    // advertises the handoff was described as replacing itself
                    // with nothing interrupted — if that fails, the failure is
                    // shown, not silently traded for the restart that kills
                    // every pane the user was just told would live. The
                    // stop-and-start path is only taken where its bloodbath is
                    // what the dialog actually said.
                    if crate::daemon::spawn::local_daemon_supports(
                        crate::daemon::protocol::FEATURE_HANDOFF,
                    ) {
                        crate::daemon::spawn::hand_off()
                    } else {
                        crate::daemon::spawn::restart()
                    }
                })
                .await;
            Self::settle_after_restart(this, restarted, cx).await;
        })
        .detach();
    }

    /// Put the window back together once the restart has been attempted, either
    /// way it went.
    ///
    /// Split into three steps because the middle one must not run with this
    /// window's entity leased. `resync_after_local_daemon_change` takes the
    /// whole `App` and rebuilds *every* local window from the tree, and the
    /// first thing it asks each one is which tabs it is showing — which it gets
    /// by reading that window's `Tty7App` back out of the registry. Called from
    /// inside `update_in`, the window it reaches for first is the one already
    /// leased to the closure, and gpui's answer to a double lease is a panic
    /// that takes the process with it: clicking Restart Server made the whole
    /// app vanish.
    async fn settle_after_restart(
        this: gpui::WeakEntity<Self>,
        restarted: anyhow::Result<()>,
        cx: &mut gpui::AsyncApp,
    ) {
        // A refused handoff leaves the daemon exactly as it was, still serving
        // the panes this window just dropped. Leaving it at the error here
        // strands them: every restore path is tree-driven, and the next sync of
        // this emptied window would diff into "close every tab" against the
        // mirror — deleting the pane records under the still-running shells, or
        // the whole workspace if the user simply closes the window first (#554).
        // So the resync below runs either way; this step only says so on screen.
        if this
            .update_in(cx, |this, window, cx| {
                if let Err(e) = &restarted {
                    log::error!("restart background service failed, resyncing from the tree: {e}");
                    let text = t_fmt(
                        L10nKey::AppRestartServerFailed,
                        &[("error", &e.to_string())],
                    );
                    this.startup_error = Some(gpui::SharedString::from(text.clone()));
                    window.push_notification(text, cx);
                }
            })
            .is_err()
        {
            return;
        }

        // The link we held pointed at the server we just killed; the reconnect
        // finds a new process whose registry knows nothing about these panes.
        // The helper drops the dead link first — a pull sent down it dies on a
        // dead socket before the reader notices — and rebuilds every local
        // window from the tree. Where the restart failed and the daemon is
        // really gone, the pull misses and the rehydration debt keeps the empty
        // window from being pushed back up.
        //
        // The invalidating helper, not the one the reconnect uses: nothing here
        // handshaked a link. Half of `hand_off`'s failures happen *after* the
        // exec — a daemon that never started listening again is gone, and the
        // client we still hold points at its socket, which `is_connected` keeps
        // calling good until its reader sees the EOF.
        cx.update(crate::ui::tree_sync::resync_after_local_daemon_change);

        let _ = this.update_in(cx, |this, window, cx| {
            this.focus_active(window, cx);
            cx.notify();
        });
    }

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
        let cfg = cx.global_mut::<Config>();
        cfg.font_size = size;
        cfg.save();
        cx.notify();
    }

    pub(crate) fn change_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.set_font_size(self.font_size + delta, cx);
    }

    pub(crate) fn reset_font_size(&mut self, cx: &mut Context<Self>) {
        self.set_font_size(Config::default().font_size, cx);
    }

    fn set_ui_font_size(&mut self, size: f32, cx: &mut Context<Self>) {
        use crate::core::config::{UI_FONT_SIZE_MAX, UI_FONT_SIZE_MIN};
        let size = size.clamp(UI_FONT_SIZE_MIN, UI_FONT_SIZE_MAX);
        let cfg = cx.global_mut::<Config>();
        if cfg.ui_font_size == size {
            return;
        }
        cfg.ui_font_size = size;
        cfg.save();
        // Unlike the settings that only redraw the window they were changed
        // in, this one re-lays-out every open window, and each reads the new
        // rem from the global on its own next frame.
        cx.refresh_windows();
        cx.notify();
    }

    pub(crate) fn ui_font_size(&self, cx: &gpui::App) -> f32 {
        cx.global::<Config>().ui_font_size
    }

    pub(crate) fn change_ui_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.set_ui_font_size(self.ui_font_size(cx) + delta, cx);
    }

    pub(crate) fn reset_ui_font_size(&mut self, cx: &mut Context<Self>) {
        self.set_ui_font_size(Config::default().ui_font_size, cx);
    }

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
        let cfg = cx.global_mut::<Config>();
        cfg.line_height = mul;
        cfg.save();
        cx.notify();
    }

    pub(crate) fn change_line_height(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.set_line_height(self.line_height + delta, cx);
    }

    pub(crate) fn reset_line_height(&mut self, cx: &mut Context<Self>) {
        self.set_line_height(Config::default().line_height, cx);
    }

    pub(crate) fn set_preset(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        // A confirmed pick ends any preview: there is nothing left to roll back.
        self.theme_preview_restore = None;
        self.write_preset(id, cx);
        self.after_theme_change(window, cx);
    }

    /// Points whichever preset slot is live at `id`, in memory only.
    fn write_preset(&mut self, id: &str, cx: &mut Context<Self>) {
        let dark_now = crate::ui::theme::system_dark(cx);
        let cfg = cx.global_mut::<Config>();
        if !cfg.theme_follow_system {
            cfg.theme_preset = id.to_string();
        } else if dark_now {
            cfg.theme_preset_dark = id.to_string();
        } else {
            cfg.theme_preset_light = id.to_string();
        }
    }

    /// Shows a preset for as long as the palette's theme picker is open, so
    /// arrowing through the list is how you find out what a theme looks like.
    /// Nothing is written to `config.json` until the pick is confirmed.
    pub(crate) fn preview_preset(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.theme_preview_restore.is_none() {
            self.theme_preview_restore = Some(crate::ui::theme::effective_preset_id(cx));
        }
        self.write_preset(id, cx);
        self.apply_theme_change(false, window, cx);
    }

    /// Puts back the preset that was live before the preview started. A no-op
    /// when nothing is being previewed.
    pub(crate) fn cancel_preset_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.theme_preview_restore.take() else {
            return;
        };
        self.write_preset(&id, cx);
        self.apply_theme_change(false, window, cx);
    }

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
            let effective = crate::ui::theme::effective_preset_id(cx);
            let cfg = cx.global_mut::<Config>();
            cfg.theme_follow_system = false;
            cfg.theme_preset = effective;
        }
        self.after_theme_change(window, cx);
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

    pub(crate) fn set_theme_legible_palette(
        &mut self,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.global_mut::<Config>().theme_legible_palette = on;
        self.after_theme_change(window, cx);
    }

    fn after_theme_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_theme_change(true, window, cx);
    }

    /// Repaints everything a theme change touches. `persist` is false for a
    /// palette preview, which repaints on every arrow key and must not turn
    /// each of those keystrokes into a `config.json` write.
    fn apply_theme_change(&mut self, persist: bool, window: &mut Window, cx: &mut Context<Self>) {
        apply_theme(Some(window), cx);
        set_menus(cx);
        if persist {
            cx.global::<Config>().save();
        }
        self.rebuild_theme_editor(window, cx);
        self.sync_window_opacity_slider(window, cx);
        cx.notify();
    }

    /// Opens or closes the theme picker, and moves the caret with it.
    ///
    /// The panel leads with a search box, and it opened unfocused — so the
    /// first thing typed at a panel whose whole job is picking one of nine
    /// themes went nowhere. Closing hands the caret back to the settings
    /// search rather than leaving it on a box that is no longer drawn.
    pub(crate) fn toggle_theme_panel(
        &mut self,
        slot: crate::ui::settings::ThemeSlot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let opened = match self.active_settings_mut() {
            Some(s) if s.theme_panel_open && s.theme_panel_slot == slot => {
                s.theme_panel_open = false;
                false
            }
            Some(s) => {
                s.theme_panel_open = true;
                s.theme_panel_slot = slot;
                true
            }
            None => return,
        };
        self.focus_theme_panel(opened, window, cx);
        cx.notify();
    }

    pub(crate) fn close_theme_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_settings_mut().is_none() {
            return;
        }
        if let Some(s) = self.active_settings_mut() {
            s.theme_panel_open = false;
        }
        self.focus_theme_panel(false, window, cx);
        cx.notify();
    }

    fn focus_theme_panel(&mut self, opened: bool, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.settings.as_ref().map(|s| match opened {
            true => s.theme_search.read(cx).focus_handle(cx),
            false => s.search.read(cx).focus_handle(cx),
        });
        if let Some(handle) = handle {
            window.focus(&handle, cx);
        }
    }

    pub(crate) fn open_themes_folder(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = crate::ui::presets::themes_dir() else {
            log::warn!("no config directory, so no themes folder to open");
            return;
        };
        // The folder is created on demand, so the first click on a fresh
        // install is also the one that can fail. Handing an absent path to the
        // file manager just opens nothing.
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn!("could not create {}: {e}", dir.display());
            crate::ui::host_ops::HostOps::notify_err(
                window,
                cx,
                &t_fmt(
                    L10nKey::OpenInFileManagerFailed,
                    &[("path", &dir.display().to_string())],
                ),
                &e,
            );
            return;
        }
        cx.open_with_system(&dir);
    }

    pub(crate) fn fork_active_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = crate::ui::theme::effective_preset_id(cx);
        let theme = crate::ui::presets::by_id(cx, &id);
        match crate::ui::presets::fork_to_file(&theme) {
            Ok(new_id) => {
                crate::ui::presets::load_registry(cx);
                self.set_preset(&new_id, window, cx);
            }
            // A button that does nothing is the worst kind of failure: there
            // is no way to tell it from "I clicked the wrong thing".
            Err(e) => {
                log::warn!("failed to duplicate theme: {e}");
                crate::ui::host_ops::HostOps::notify_err(
                    window,
                    cx,
                    t(L10nKey::ThemeDuplicateFailed),
                    &e,
                );
            }
        }
    }

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
            // Every colour edit runs through here. Without this the picker
            // moves, the theme does not, and nothing says why.
            log::warn!("failed to write theme file: {e}");
            crate::ui::host_ops::HostOps::notify_err(window, cx, t(L10nKey::ThemeSaveFailed), &e);
            return;
        }
        crate::ui::presets::load_registry(cx);
        apply_theme(Some(window), cx);
        cx.notify();
    }

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

    pub(crate) fn effective_window_opacity(cx: &App) -> f32 {
        let config = cx.global::<Config>();
        let theme = crate::ui::presets::by_id(cx, &crate::ui::theme::effective_preset_id(cx));
        let blur = config.window_blur.unwrap_or(theme.blur);
        config.window_opacity.or(theme.opacity).unwrap_or_else(|| {
            crate::ui::theme::default_window_opacity(config.window_backdrop, blur)
        })
    }

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

    #[cfg(target_os = "windows")]
    pub(crate) fn set_window_backdrop(
        &mut self,
        backdrop: WindowBackdrop,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.global_mut::<Config>().window_backdrop = backdrop;
        apply_theme(Some(window), cx);
        cx.global::<Config>().save();
        // A material changes the default opacity (SYSTEM_MATERIAL_OPACITY
        // vs 1.0), so the slider must track the new effective value.
        self.sync_window_opacity_slider(window, cx);
        // Rebuild the rows as well as the selected index. The previous value
        // may have been an unsupported preset retained only for cross-machine
        // config sync, and must disappear after the user selects a supported
        // preset on this machine.
        self.sync_window_backdrop_select(window, cx);
        cx.notify();
    }

    pub(crate) fn reset_window_overrides(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        {
            let config = cx.global_mut::<Config>();
            clear_window_override_values(config, cfg!(target_os = "windows"));
        }
        apply_theme(Some(window), cx);
        cx.global::<Config>().save();
        self.sync_window_opacity_slider(window, cx);
        #[cfg(target_os = "windows")]
        self.sync_window_backdrop_select(window, cx);
        cx.notify();
    }

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

    #[cfg(target_os = "windows")]
    pub(crate) fn sync_window_backdrop_select(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(select) = self
            .active_settings()
            .map(|s| s.window_backdrop_select.clone())
        {
            let current = cx.global::<Config>().window_backdrop;
            let rows = window_backdrop_labels(current);
            let selected = window_backdrop_index(current);
            select.update(cx, |state, cx| {
                state.set_items(SearchableVec::new(rows), window, cx);
                // Replacing the delegate clears its selection snapshot, so
                // restore the stored value after installing the new rows.
                state.set_selected_index(Some(IndexPath::default().row(selected)), window, cx);
            });
        }
    }

    pub(crate) fn pick_theme_image(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.mutate_active_theme(
                        |theme| {
                            let opacity = theme.image.as_ref().map(|i| i.opacity).unwrap_or(0.3);
                            theme.image = Some(crate::ui::presets::Image { path, opacity });
                        },
                        window,
                        cx,
                    );
                    this.rebuild_theme_editor(window, cx);
                });
            }
        })
        .detach();
    }

    pub(crate) fn remove_theme_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mutate_active_theme(|theme| theme.image = None, window, cx);
        self.rebuild_theme_editor(window, cx);
    }

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
        let seed_specs: [(ThemeEdit, u32); 5] = [
            (ThemeEdit::Background, theme.background_color()),
            (ThemeEdit::Foreground, theme.foreground),
            (ThemeEdit::Accent, theme.accent),
            (ThemeEdit::Cursor, theme.caret.unwrap_or(theme.accent)),
            (ThemeEdit::Selection, neutrals.selection),
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
            .map(|&(edit, value)| (edit, make(edit, value, &mut subs, cx)))
            .collect();
        let ansi = (0..16)
            .map(|i| {
                let (r, g, b) = theme.ansi16[i];
                let value = (r as u32) << 16 | (g as u32) << 8 | b as u32;
                (
                    ThemeEdit::Ansi(i),
                    make(ThemeEdit::Ansi(i), value, &mut subs, cx),
                )
            })
            .collect();

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

    pub(crate) fn set_font_ligatures(&mut self, on: bool, cx: &mut Context<Self>) {
        let features = on.then(|| {
            crate::core::config::FontFeatures(Arc::new(vec![
                ("calt".to_string(), 1),
                ("liga".to_string(), 1),
            ]))
        });
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

    pub(crate) fn set_cursor_style(&mut self, style: ConfigCursorStyle, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.cursor_style = style);
        let cfg = cx.global::<Config>().clone();
        self.terminal_cursor_style = cfg.cursor_style;
        self.terminal_scrollback_limit = cfg.scrollback_limit;
        self.apply_terminal_config_to_panes(&cfg, cx);
    }

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

    pub(crate) fn set_link_file_open(
        &mut self,
        mode: crate::core::config::LinkFileOpen,
        cx: &mut Context<Self>,
    ) {
        self.update_config(cx, |cfg| cfg.link_file_open = Some(mode));
    }

    pub(crate) fn set_ssh_loopback_forward(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.ssh_loopback_forward = on);
    }

    pub(crate) fn set_verify_host_keys(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.verify_host_keys = on);
    }

    pub(crate) fn set_ssh_warn_on_close(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.ssh_warn_on_close = on);
    }

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

    pub(crate) fn refresh_managed_forwards(&mut self, pane_id: u64, cx: &mut Context<Self>) {
        self.loopback_panel.managed = self.forward_route(pane_id, cx).list();
        cx.notify();
    }

    pub(crate) fn set_managed_forward_kind(
        &mut self,
        kind: crate::daemon::protocol::SshForwardKind,
        cx: &mut Context<Self>,
    ) {
        self.loopback_panel.mf_kind = kind;
        cx.notify();
    }

    /// The managed-forward form's fields as plain text, for the two callers
    /// that have to agree on what they add up to.
    pub(crate) fn managed_forward_fields(&self, cx: &gpui::App) -> ForwardFields {
        let val = |input: &Entity<InputState>| input.read(cx).value().to_string();
        ForwardFields {
            kind: self.loopback_panel.mf_kind,
            bind_host: val(&self.loopback_panel.mf_bind_host),
            bind_port: val(&self.loopback_panel.mf_bind_port),
            target_host: val(&self.loopback_panel.mf_target_host),
            target_port: val(&self.loopback_panel.mf_target_port),
            description: val(&self.loopback_panel.mf_description),
        }
    }

    pub(crate) fn add_managed_forward(
        &mut self,
        pane_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::daemon::protocol::ForwardStatus;

        let Some(rule) = self.managed_forward_fields(cx).collect() else {
            // Add is disabled while the fields do not make a rule and the form
            // already says what is missing, so there is nothing to do here and
            // nothing left to explain.
            return;
        };
        let route = self.forward_route(pane_id, cx);
        let previous = self.loopback_panel.mf_editing.clone();
        // A saved edit is a replace, and the rule being replaced has to come
        // out first: the ordinary edit keeps the bind port, and the far side
        // really does bind it, so adding first would collide with the very
        // rule it is replacing and fail every edit that only renames a rule or
        // moves its target.
        if let Some(old) = &previous {
            let Some(list) = route.remove(old.id) else {
                // Nothing came back, so what the far side still has is
                // unknown — most likely the old rule, still listening. Adding
                // on top of that would collide with it, and putting it back
                // afterwards would leave two of it. Stop while nothing has
                // changed.
                self.loopback_panel.mf_error = Some(t(L10nKey::ForwardRequestFailed).to_string());
                cx.notify();
                return;
            };
            self.loopback_panel.managed = list;
        }

        let before: Vec<u64> = self.loopback_panel.managed.iter().map(|m| m.id).collect();
        let mut failure = None;
        match route.add(rule) {
            // The request never got an answer. An empty list here is not "this
            // pane has no forwards", it is "nobody said" — assigning it is what
            // used to blank the panel on a dropped connection.
            None => failure = Some(t(L10nKey::ForwardRequestFailed).to_string()),
            Some(list) => {
                // A rule that could not be started is registered all the same,
                // with the reason in its status, so whether the add worked is a
                // question about the entry it appended rather than about
                // whether the call returned.
                let broken = added_forward(&before, &list).and_then(|added| match &added.status {
                    ForwardStatus::Error(msg) => Some((added.id, msg.clone())),
                    ForwardStatus::Listening => None,
                });
                self.loopback_panel.managed = list;
                if let Some((id, msg)) = broken {
                    if let Some(list) = route.remove(id) {
                        self.loopback_panel.managed = list;
                    }
                    failure = Some(msg);
                }
            }
        }

        if let Some(msg) = failure {
            // Put back what the edit took out, so the worst a failed Save can
            // do is leave everything exactly as it was — with the form still
            // open on the rule and the reason underneath it.
            if let Some(old) = &previous {
                let before: Vec<u64> = self.loopback_panel.managed.iter().map(|m| m.id).collect();
                if let Some(list) = route.add(rule_of(old)) {
                    // The rule comes back under a new id and the form is still
                    // editing it, so the form has to be pointed at the entry
                    // that now exists — otherwise the next Save would remove
                    // an id nobody has and add a second copy of the rule.
                    if let Some(restored) = added_forward(&before, &list) {
                        self.loopback_panel.mf_editing = Some(restored.clone());
                    }
                    self.loopback_panel.managed = list;
                }
            }
            self.loopback_panel.mf_error = Some(msg);
            cx.notify();
            return;
        }

        self.loopback_panel.mf_editing = None;
        self.loopback_panel.mf_error = None;
        self.loopback_panel.form_pane_id = None;
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

    pub(crate) fn edit_managed_forward(
        &mut self,
        forward: crate::daemon::protocol::ManagedForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loopback_panel.mf_kind = forward.kind;
        self.loopback_panel.form_pane_id = Some(forward.pane_id);
        self.loopback_panel.mf_error = None;
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
        self.loopback_panel.mf_editing = Some(forward);
        cx.notify();
    }

    pub(crate) fn cancel_managed_forward_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loopback_panel.mf_editing = None;
        self.loopback_panel.mf_error = None;
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

    pub(crate) fn remove_managed_forward(
        &mut self,
        pane_id: u64,
        forward_id: u64,
        cx: &mut Context<Self>,
    ) {
        // Only what the far side actually answered with. A request that never
        // got a reply knows nothing about the remaining forwards, and writing
        // its empty list into the panel would blank a list that is still there.
        if let Some(list) = self.forward_route(pane_id, cx).remove(forward_id) {
            self.loopback_panel.managed = list;
        }
        cx.notify();
    }

    pub(crate) fn show_ssh_forwards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pane_id, _)) = self.active_connected_native_ssh_pane(window, cx) else {
            return;
        };
        self.set_right_panel_tab(crate::core::config::RightPanelTab::Info, cx);
        if self.loopback_panel.form_pane_id != Some(pane_id) {
            self.toggle_managed_forward_form(pane_id, window, cx);
        }
    }

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

    pub(crate) fn close_managed_forward_form(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loopback_panel.form_pane_id = None;
        self.cancel_managed_forward_edit(window, cx);
    }

    fn open_typed_ssh_connect(&mut self, input: &str, window: &mut Window, cx: &mut Context<Self>) {
        match parse_ssh_connect_input(input) {
            Ok(parsed) => {
                let resolved = ssh_config::resolve_alias_to_profile(&parsed.profile.host);
                let (profile, proxy_jump) = merge_typed_ssh_over_alias(parsed, resolved);
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

    pub(crate) fn set_check_for_updates(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.check_for_updates = on);
    }

    pub(crate) fn set_auto_download_updates(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.auto_download_updates = on);
    }

    /// Moves this installation to another release feed. See
    /// `update::switch_channel` for what a switch invalidates, and why moving
    /// back to Stable does not roll the running build back.
    pub(crate) fn set_update_channel(
        &mut self,
        channel: crate::core::config::UpdateChannel,
        cx: &mut Context<Self>,
    ) {
        if cx.global::<Config>().update_channel == channel {
            return;
        }
        self.update_config(cx, |cfg| cfg.update_channel = channel);
        crate::core::update::switch_channel(cx);
    }

    /// Takes effect at next launch: `core::cli_install` runs once from `main`,
    /// before there is a window to flip this in. Turning it off does not remove
    /// a symlink already placed — the install is idempotent, not reversible.
    pub(crate) fn set_install_cli_on_path(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.install_cli_on_path = on);
    }

    pub(crate) fn set_dim_inactive_panes(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.dim_inactive_panes = on);
    }

    pub(crate) fn set_cursor_blink(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.cursor_blink = on);
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

    pub(crate) fn set_tab_bar_position(&mut self, pos: TabBarPosition, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.tab_bar_position = pos);
    }

    pub(crate) fn set_sidebar_grouping(
        &mut self,
        grouping: crate::core::config::SidebarGrouping,
        cx: &mut Context<Self>,
    ) {
        self.update_config(cx, |cfg| cfg.sidebar_grouping = grouping);
    }

    pub(crate) fn set_sidebar_diff_preview(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.sidebar_diff_preview = on);
    }

    pub(crate) fn toggle_tab_sidebar(&mut self, cx: &mut Context<Self>) {
        let next = match cx.global::<Config>().tab_bar_position {
            TabBarPosition::Top => TabBarPosition::Left,
            TabBarPosition::Left => TabBarPosition::Top,
        };
        self.set_tab_bar_position(next, cx);
    }

    pub(crate) fn toggle_left_panel(&mut self, cx: &mut Context<Self>) {
        let (pos, collapsed) = match cx.global::<Config>().tab_bar_position {
            TabBarPosition::Top => (TabBarPosition::Left, false),
            TabBarPosition::Left => (TabBarPosition::Left, !self.sidebar_collapsed),
        };
        self.sidebar_collapsed = collapsed;
        self.update_config(cx, |cfg| {
            cfg.tab_bar_position = pos;
            cfg.sidebar_collapsed = collapsed;
        });
        cx.notify();
    }

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

    pub(crate) fn set_notify_threshold(&mut self, secs: u64, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.notify_threshold_secs = secs.clamp(1, 3600));
    }

    pub(crate) fn set_bell_mode(
        &mut self,
        mode: crate::core::config::BellMode,
        cx: &mut Context<Self>,
    ) {
        self.update_config(cx, |cfg| cfg.bell = mode);
    }

    pub(crate) fn set_restore_session(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.restore_session = on);
    }

    /// Takes effect on the next pane: a shell is told where its history lives
    /// when it starts, and nothing can move it afterwards.
    pub(crate) fn set_per_pane_history(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.per_pane_history = on);
    }

    pub(crate) fn set_show_tray_icon(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.show_tray_icon = on);
    }

    pub(crate) fn set_macos_option_as_alt(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.macos_option_as_alt = on);
    }

    pub(crate) fn set_mouse_hide_while_typing(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.mouse_hide_while_typing = on);
        crate::ui::theme::apply_cursor_hide_mode(cx);
    }

    pub(crate) fn set_focus_follows_mouse(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.focus_follows_mouse = on);
    }

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

    pub(crate) fn set_smooth_scroll(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.smooth_scroll = on);
    }

    /// Panes read the modifier off the global config as each wheel event
    /// arrives, so there is nothing to push at them here.
    pub(crate) fn set_mouse_zoom_modifier(
        &mut self,
        modifier: MouseZoomModifier,
        cx: &mut Context<Self>,
    ) {
        self.update_config(cx, |cfg| cfg.mouse_zoom_modifier = modifier);
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

    /// Hands the prompt to the shell's own line editor, or takes it back.
    ///
    /// Live panes carry a cached copy of the flag (see
    /// [`crate::terminal::view::TerminalView::prompt_editor`]), so the switch
    /// has to reach them: without this, only panes opened afterwards would
    /// change hands.
    pub(crate) fn set_prompt_editor(&mut self, on: bool, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.prompt_editor = on);
        for tab in &self.tabs {
            for leaf in tab.pane.terminals() {
                leaf.update(cx, |v, cx| v.set_prompt_editor(on, cx));
            }
        }
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

    pub(crate) fn sync_window_title(&self, window: &mut Window, cx: &App) {
        let title = WorkspaceStore::all(cx)
            .get(self.workspace)
            .filter(|w| crate::ui::machine_mirror::pane_count(cx, w).unwrap_or(0) > 0)
            .and_then(|w| crate::ui::machine_mirror::display_name(cx, w))
            .unwrap_or_else(|| "tty7".to_string());
        if *self.window_title.borrow() == title {
            return;
        }
        window.set_window_title(&title);
        *self.window_title.borrow_mut() = title;
    }

    pub(crate) fn focus_active(&self, window: &mut Window, cx: &mut App) {
        self.sync_window_title(window, cx);
        if let Some(settings) = self.settings.as_ref() {
            window.focus(&settings.focus_handle, cx);
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            window.focus(&self.home_focus, cx);
            return;
        };
        if let Some(overlay) = tab.diff_overlay.as_ref() {
            window.focus(&overlay.focus_handle, cx);
            return;
        }
        if let Some(leaf) = tab.focus_target() {
            let handle = leaf.focus_handle(cx);
            window.focus(&handle, cx);
        }
    }

    pub(crate) fn remember_active_pane(&mut self, window: &Window, cx: &App) {
        let active = self.active;
        if let Some(tab) = self.tabs.get_mut(active)
            && let Some(leaf) = tab.pane.focused_leaf(window, cx)
        {
            tab.last_focused = Some(leaf.entity_id());
        }
    }

    fn focus_leaf(&self, leaf: &PaneSlot, window: &mut Window, cx: &mut App) {
        let handle = leaf.focus_handle(cx);
        window.focus(&handle, cx);
    }

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
            Err(reason) => {
                pending.update(cx, |p, cx| p.fail(reason, cx));
                return;
            }
        };
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
        let was_focused = pending.read(cx).focus_handle.contains_focused(window, cx);
        let resume = (!parts.restored)
            .then(|| {
                let spawn = &pending.read(cx).spawn;
                agent_resume_command(
                    &spawn.agent,
                    spawn.agent_session_id.as_deref(),
                    spawn.agent_launch_argv.as_deref(),
                    cx,
                )
            })
            .flatten();
        let view = build_terminal_view(parts, font_size, window, cx);
        if let Some(cmd) = resume {
            view.read(cx).run_command_line(&cmd);
        }
        let slot = PaneSlot::Ready(view.clone());
        self.tabs
            .iter_mut()
            .any(|tab| tab.pane.replace_leaf(slot_id, slot.clone()));
        if was_focused {
            self.focus_leaf(&slot, window, cx);
        }
        self.save_session(cx);
        cx.notify();
    }

    fn refresh_git_status_all(&mut self, cx: &mut Context<Self>) {
        for leaf in self.tabs.iter().flat_map(|tab| tab.pane.terminals()) {
            leaf.update(cx, |view, cx| view.refresh_git_status_now(cx));
        }
    }

    fn new_tab_insert_at(&self, cx: &App) -> usize {
        match cx.global::<Config>().new_tab_position {
            NewTabPosition::AfterCurrent => (self.active + 1).min(self.tabs.len()),
            NewTabPosition::End => self.tabs.len(),
        }
    }

    pub(crate) fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_tab_with_shell(None, window, cx);
    }

    pub(crate) fn new_tab_at(
        &mut self,
        cwd: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_tab_with_cwd(Some(cwd), None, window, cx);
    }

    pub(crate) fn new_tab_with_shell(
        &mut self,
        shell: Option<ShellSpec>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = self.tabs.get(self.active).and_then(|t| {
            t.pane
                .focused_or_first(window, cx)
                .and_then(|leaf| leaf.read(cx).spawnable_cwd())
        });
        self.new_tab_with_cwd(cwd, shell, window, cx);
    }

    /// A shell taken out of the new-tab menu, wherever the ⌥ key said to put
    /// it.
    pub(crate) fn open_shell(
        &mut self,
        shell: Option<ShellSpec>,
        at: SpawnWhere,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match at {
            SpawnWhere::NewTab => self.new_tab_with_shell(shell, window, cx),
            SpawnWhere::Split => {
                self.split_into(Axis::Horizontal, Some(SpawnAs::Shell(shell)), window, cx)
            }
        }
    }

    fn new_tab_with_cwd(
        &mut self,
        cwd: Option<std::path::PathBuf>,
        shell: Option<ShellSpec>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.guard_local_spawn(window, cx) {
            return;
        }
        let group = self.spawn_group(cwd.as_deref(), cx);
        let tab = match new_terminal(
            self.window_workspace(cx),
            Some(self.workspace),
            self.font_size,
            cwd,
            None,
            shell,
            window,
            cx,
        ) {
            Ok(view) => view,
            Err(e) => {
                log::error!("new tab spawn failed: {e}");
                let text = t_fmt(L10nKey::AppOpenTerminalFailed, &[("error", &e.to_string())]);
                // A retry from the home screen fails the same way; keep the
                // reason on screen rather than only in a toast that leaves.
                self.startup_error = Some(gpui::SharedString::from(text.clone()));
                window.push_notification(text, cx);
                cx.notify();
                return;
            }
        };
        // Something opened, so whatever the last failure was is stale.
        self.startup_error = None;
        self.remember_active_pane(window, cx);
        self.maximized = None;
        let insert_at = self.new_tab_insert_at(cx);
        let new_tab = Tab::new(Pane::leaf(tab));
        if let Some(group) = group {
            *new_tab.sidebar_group.borrow_mut() = group;
        }
        self.tabs.insert(insert_at, new_tab);
        self.active = insert_at;
        self.focus_active(window, cx);
        self.save_session(cx);
        cx.notify();
    }

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
                window.push_notification(
                    t_fmt(
                        L10nKey::AppSshConnectionFailed,
                        &[("error", &e.to_string())],
                    ),
                    cx,
                );
                return;
            }
        };
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
                window.push_notification(
                    t_fmt(L10nKey::AppSshReconnectFailed, &[("error", &e.to_string())]),
                    cx,
                );
                return;
            }
        };
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

    pub(crate) fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        self.split_into(axis, None, window, cx);
    }

    /// `spawn` of `None` is what ⌘D has always done: copy the pane being split
    /// — same shell, or the same host dialled again. A `Some` names the new
    /// pane outright, which is how a row taken out of the new-tab menu with ⌥
    /// held lands beside the current pane instead of in a tab of its own.
    pub(crate) fn split_into(
        &mut self,
        axis: Axis,
        spawn: Option<SpawnAs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
        else {
            return;
        };
        if !self.guard_local_spawn(window, cx) {
            return;
        }
        let cwd = target.read(cx).spawnable_cwd();
        let spawn = match spawn {
            Some(spawn) => spawn,
            // A stored spec is resolved against the saved host before it is
            // dialled; one handed in by a caller was just built from that host
            // and needs no second pass.
            None => match target.read(cx).ssh_spec() {
                Some(spec) => {
                    SpawnAs::Ssh(crate::ui::ssh_connect::resolve_persisted_ssh_spec(spec, cx))
                }
                None => SpawnAs::Shell(target.read(cx).shell_spec()),
            },
        };
        let new = match spawn {
            SpawnAs::Ssh(spec) => {
                match new_terminal_native(self.font_size, cwd, spec, window, cx) {
                    Ok(view) => PaneSlot::Ready(view),
                    Err(e) => {
                        log::error!("native SSH split spawn failed: {e}");
                        window.push_notification(
                            t_fmt(
                                L10nKey::AppSshConnectionFailed,
                                &[("error", &e.to_string())],
                            ),
                            cx,
                        );
                        return;
                    }
                }
            }
            SpawnAs::Shell(shell) => {
                match new_terminal(
                    self.window_workspace(cx),
                    Some(self.workspace),
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
                        window.push_notification(
                            t_fmt(L10nKey::AppSplitPaneFailed, &[("error", &e.to_string())]),
                            cx,
                        );
                        return;
                    }
                }
            }
        };
        if let Some(tab) = self.tabs.get_mut(self.active)
            && tab
                .pane
                .split_leaf(target.entity_id(), axis, false, new.clone())
        {
            self.maximized = None;
            self.focus_leaf(&new, window, cx);
            self.save_session(cx);
            cx.notify();
        }
    }

    fn close_pane(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_pane_inner(false, window, cx);
    }

    /// `confirmed` is set only by the answer to this pane's own close question,
    /// so it travels with the close rather than being read back off shared
    /// state a second, unrelated close could have overwritten.
    fn close_pane_inner(&mut self, confirmed: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !confirmed && let Some(reason) = self.focused_pane_close_reason(window, cx) {
            self.ask_before_closing(CloseTarget::Pane, reason, window, cx);
            return;
        }
        self.maximized = None;
        let focused = self.tabs.get(self.active).and_then(|tab| {
            tab.pane
                .leaves()
                .into_iter()
                .find(|l| l.contains_focused(window, cx))
                .and_then(|l| l.terminal().cloned())
        });
        let outcome = match self.tabs.get_mut(self.active) {
            Some(tab) => tab.pane.close_focused(window, cx),
            None => return,
        };
        match outcome {
            // The last pane takes its tab with it. The question was already
            // asked about this very pane, so carry the answer across rather
            // than letting the tab re-derive the same reason and ask again.
            CloseOutcome::RemoveSelf => {
                self.close_tab_inner(self.active, confirmed, window, cx);
            }
            CloseOutcome::NotFound => {
                let single = self
                    .tabs
                    .get(self.active)
                    .is_some_and(|tab| tab.pane.leaves().len() <= 1);
                if single {
                    self.close_tab_inner(self.active, confirmed, window, cx);
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
            return;
        };
        if view.read(cx).ssh_disconnected() {
            cx.notify();
            return;
        }
        match self.tabs[index].pane.close_leaf(view.entity_id()) {
            CloseOutcome::RemoveSelf => self.close_tab(index, window, cx),
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

    fn cycle_pane(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
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
        if let Some(tab) = self.tabs.get_mut(self.active)
            && tab.pane.swap_leaf_indices(from, to)
        {
            self.maximized = None;
            self.save_session(cx);
            cx.notify();
        }
    }

    /// The patch of the layout a pane being dragged would land on, lit up.
    ///
    /// Also records that landing as the one a drop would take, so the drop and
    /// the highlight can never disagree: a zone the tree refuses to carry out
    /// is neither drawn nor remembered, and releasing over it does nothing.
    fn pane_landing(&self, window: &Window, cx: &App) -> Option<gpui::AnyElement> {
        use crate::ui::pane_drag;

        let from = pane_drag::lifted(&self.pane_drag)?;
        let area = self.pane_area.get()?;
        let tab = self.tabs.get(self.active)?;
        let leaves = tab.pane.leaves();
        let slot = leaves.iter().find(|l| l.entity_id() == from)?;
        let bounds = pane_drag::leaf_bounds(&tab.pane, area);
        let zone = pane_drag::zone_at(area, &bounds, window.mouse_position())?;
        // The zone comes back naming its target by position, which only means
        // anything against this frame's leaves. Drawn against the panes here,
        // and remembered as the panes so the drop that reads it back a frame
        // later is looking for the same ones.
        let here = zone.map(|i| leaves.get(i).cloned())?;
        let pinned = zone.map(|i| leaves.get(i).map(|l| l.entity_id()))?;
        let rect = pane_drag::landing(&tab.pane, slot, here, area)?;
        pane_drag::set_landing(&self.pane_drag, pinned);

        let accent = cx.theme().drag_border;
        Some(
            div()
                .absolute()
                .left(rect.origin.x - area.origin.x)
                .top(rect.origin.y - area.origin.y)
                .w(rect.size.width)
                .h(rect.size.height)
                .rounded(px(6.))
                .border_2()
                .border_color(accent)
                .bg(accent.opacity(0.15))
                .into_any_element(),
        )
    }

    /// The patch of the layout a tab held over it would be grafted into, lit
    /// up — and, while that is on offer, the strip held still underneath it.
    ///
    /// A tab is dragged with the same grip that reorders it, so the two
    /// readings share one gesture: over the strip or the sidebar it is a
    /// reorder, out over the panes it is a merge. Suspending the reorder is
    /// what keeps the drop from being both.
    fn tab_landing(&self, window: &Window, cx: &App) -> Option<gpui::AnyElement> {
        self.tab_merge.set(None);
        let dragged = crate::ui::reorder::dragged_tab(&self.reorder);
        let offer = dragged.and_then(|id| self.tab_landing_rect(id, window));
        crate::ui::reorder::suspend(&self.reorder, offer.is_some());
        let (zone, rect) = offer?;
        let area = self.pane_area.get()?;
        self.tab_merge.set(Some((dragged?, zone)));

        let accent = cx.theme().drag_border;
        Some(
            div()
                .absolute()
                .left(rect.origin.x - area.origin.x)
                .top(rect.origin.y - area.origin.y)
                .w(rect.size.width)
                .h(rect.size.height)
                .rounded(px(6.))
                .border_2()
                .border_color(accent)
                .bg(accent.opacity(0.15))
                .into_any_element(),
        )
    }

    /// Where the tab `id` would land in the tab on screen, if it can land there
    /// at all — it cannot land in itself, and there is nothing to land in while
    /// a pane is zoomed over the layout.
    fn tab_landing_rect(
        &self,
        id: tty7_core::core::machine::TabId,
        window: &Window,
    ) -> Option<(
        crate::ui::pane_drag::DropZone<gpui::EntityId>,
        Bounds<Pixels>,
    )> {
        use crate::ui::pane_drag;

        if self.maximized.is_some() {
            return None;
        }
        let area = self.pane_area.get()?;
        let host = self.tabs.get(self.active)?;
        if host.tree_id.get() == id {
            return None;
        }
        let sub = &self.tabs.iter().find(|t| t.tree_id.get() == id)?.pane;
        let leaves = host.pane.leaves();
        let bounds = pane_drag::leaf_bounds(&host.pane, area);
        let zone = pane_drag::tab_zone_at(area, &bounds, window.mouse_position())?;
        // Named by pane rather than by position for the drop a frame later,
        // the same way a pane drag's landing is.
        let here = zone.map(|i| leaves.get(i).cloned())?;
        let pinned = zone.map(|i| leaves.get(i).map(|l| l.entity_id()))?;
        let rect = pane_drag::graft_landing(&host.pane, sub, here, area)?;
        Some((pinned, rect))
    }

    /// Merges a dragged tab into the tab on screen, where the last painted
    /// frame said it would go.
    ///
    /// The panes come across as they were arranged, and the tab they came from
    /// goes away with them — nothing is killed and nothing is respawned, so a
    /// shell mid-command carries on through the move. What the source tab held
    /// besides panes is the tab's own: its name goes, and its overlays come
    /// along only where the tab receiving them has none of its own to lose.
    fn merge_tab(
        &mut self,
        source: tty7_core::core::machine::TabId,
        zone: crate::ui::pane_drag::DropZone<gpui::EntityId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pane_hover.set(None);
        let Some(from) = self.tabs.iter().position(|t| t.tree_id.get() == source) else {
            return;
        };
        let Some(host) = self.tabs.get(self.active) else {
            return;
        };
        if host.tree_id.get() == source {
            return;
        }
        let leaves = host.pane.leaves();
        let Some(zone) = zone.map(|id| leaves.iter().find(|l| l.entity_id() == id).cloned()) else {
            return;
        };

        let mut moved = self.tabs.remove(from);
        if from < self.active {
            self.active -= 1;
        }
        // Lifted out rather than cloned: these are the live terminals, and a
        // graft that finds nowhere to put them has to be able to hand them
        // back intact.
        let sub = std::mem::replace(&mut moved.pane, crate::ui::pane::Pane::Empty);
        let first = sub.first_leaf();
        let grafted = match self.tabs.get_mut(self.active) {
            Some(host) => crate::ui::pane_drag::graft(&mut host.pane, sub, zone),
            None => Err(sub),
        };
        if let Err(sub) = grafted {
            moved.pane = sub;
            self.tabs.insert(from, moved);
            if from <= self.active {
                self.active += 1;
            }
            return;
        }
        let host = &mut self.tabs[self.active];
        if host.code.is_none() {
            host.code = moved.code;
        }
        if host.diff_overlay.is_none() {
            host.diff_overlay = moved.diff_overlay;
        }
        if self
            .renaming
            .as_ref()
            .is_some_and(|r| !self.tabs.iter().any(|t| t.tree_id.get() == r.tab))
        {
            self.renaming = None;
        }
        self.maximized = None;
        if let Some(leaf) = first {
            self.focus_leaf(&leaf, window, cx);
        }
        self.save_session(cx);
        cx.notify();
    }

    /// The caret between two tabs where a pane carried up to the strip — or
    /// out to the sidebar — would become a tab of its own.
    ///
    /// The reverse of grafting a tab in, and read the same way: offered only
    /// when the drop would change something, which for a detach means the pane
    /// has somewhere to leave. The last pane in a tab is already a tab, so
    /// nothing lights up for it.
    fn detach_caret(&self, window: &Window, cx: &App) -> Option<gpui::AnyElement> {
        self.pane_detach.set(None);
        let pane = crate::ui::pane_drag::lifted(&self.pane_drag)?;
        let tab = self.tabs.get(self.active)?;
        if tab.pane.leaves().len() < 2 {
            return None;
        }
        if !tab.pane.leaves().iter().any(|l| l.entity_id() == pane) {
            return None;
        }
        // The pane's own landing is read later in the frame and answers nothing
        // while the pointer is up here, so the two are never both on offer;
        // the drop takes this one first regardless.
        let (at, caret) = self.detach_slot(window, cx)?;
        self.pane_detach.set(Some((pane, at)));

        let accent = cx.theme().drag_border;
        Some(
            div()
                .absolute()
                .left(caret.origin.x)
                .top(caret.origin.y)
                .w(caret.size.width)
                .h(caret.size.height)
                .rounded_full()
                .bg(accent)
                .into_any_element(),
        )
    }

    /// Which gap between tabs the pointer is in, as the tab a newcomer would
    /// be inserted before and the caret marking it — on the strip, or on the
    /// sidebar, whichever the pointer is over.
    ///
    /// `viewport` and the pointer are both in the window's outer coordinates.
    fn detach_slot(&self, window: &Window, cx: &App) -> Option<(usize, Bounds<Pixels>)> {
        /// How far above its first row the sidebar's band starts, so the gap
        /// over that row is inside it.
        const BAND_REACH: f32 = 6.;

        let pointer = window.mouse_position();
        // The two surfaces never share a window: the sidebar *is* the tab bar
        // when it is up, and the strip is what stands in for it when it is not.
        let vertical = matches!(cx.global::<Config>().tab_bar_position, TabBarPosition::Left)
            && !self.tabs.is_empty();
        let viewport = window.viewport_size();
        let pad = gpui_component::window_paddings(window);
        // The band each surface claims, read off the tabs it drew rather than
        // measured as an element of its own: the chips and the rows are the
        // only part of either surface a drop has anything to say about, and a
        // band derived from them cannot disagree with the gaps measured inside
        // it. Stretched past the outermost tab so the empty space beyond —
        // where the strip keeps its New Tab button, where the sidebar keeps
        // nothing at all — still reads as "after everything".
        let band = |axis: Axis, drawn: &[(usize, Bounds<Pixels>)]| {
            let top = drawn.iter().map(|(_, b)| b.origin.y).reduce(Pixels::min)?;
            let left = drawn.iter().map(|(_, b)| b.origin.x).reduce(Pixels::min)?;
            let right = drawn
                .iter()
                .map(|(_, b)| b.origin.x + b.size.width)
                .reduce(Pixels::max)?;
            Some(match axis {
                Axis::Horizontal => strip_band(viewport, pad),
                Axis::Vertical => Bounds {
                    origin: point(left, top - px(BAND_REACH)),
                    size: size(
                        right - left,
                        (viewport.height - top + px(BAND_REACH)).max(px(0.)),
                    ),
                },
            })
        };

        // Sorted by where they were drawn rather than by tab: the sidebar
        // groups its rows, and a strip that has scrolled shows a window of
        // chips. What the pointer is between is a matter of the screen.
        let measured = |slots: &[Bounds<Pixels>]| -> Vec<(usize, Bounds<Pixels>)> {
            slots
                .iter()
                .enumerate()
                .filter(|(_, b)| b.size.width > px(0.) && b.size.height > px(0.))
                .map(|(i, b)| (i, *b))
                .collect()
        };
        let surfaces = [
            (!vertical).then(|| (Axis::Horizontal, measured(&self.strip_slots.borrow()))),
            self.sidebar_open(cx)
                .then(|| (Axis::Vertical, measured(&self.sidebar_slots.borrow()))),
        ];
        let (axis, mut drawn) = surfaces
            .into_iter()
            .flatten()
            .find(|(axis, drawn)| band(*axis, drawn).is_some_and(|b| b.contains(&pointer)))?;
        let lead = |b: &Bounds<Pixels>| match axis {
            Axis::Horizontal => b.origin.x,
            Axis::Vertical => b.origin.y,
        };
        drawn.sort_by(|(_, a), (_, b)| lead(a).as_f32().total_cmp(&lead(b).as_f32()));

        let row: Vec<Bounds<Pixels>> = drawn.iter().map(|(_, b)| *b).collect();
        let (gap, caret) = crate::ui::pane_drag::insertion(&row, axis, pointer)?;
        // The gap counts tabs on screen; what an insert needs is a place in the
        // list. Past the last tab drawn is the end of the list, which is not
        // the same thing as the last tab drawn plus one: a strip too narrow to
        // show every chip has tabs on either side of the ones it drew.
        let at = drawn.get(gap).map(|(i, _)| *i).unwrap_or(self.tabs.len());
        Some((at, caret))
    }

    /// Takes a dragged pane out of its tab and gives it one of its own, where
    /// the last painted frame's caret said.
    ///
    /// Nothing is spawned and nothing is killed: the pane that leaves is the
    /// same pane, still running whatever it was running.
    fn detach_pane(
        &mut self,
        pane: gpui::EntityId,
        at: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pane_hover.set(None);
        // Which pane the tab being left comes back to, settled while the one
        // that is leaving is still in it to be ruled out.
        self.remember_active_pane(window, cx);
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let Some(slot) = tab
            .pane
            .leaves()
            .into_iter()
            .find(|l| l.entity_id() == pane)
        else {
            return;
        };
        // Refused for the last pane in a tab, which is what makes a drop that
        // would change nothing do nothing.
        let Some(slot) = tab.pane.take_leaf(&slot) else {
            return;
        };
        let cwd = slot
            .terminal()
            .and_then(|view| view.read(cx).spawnable_cwd());
        let group = self.spawn_group(cwd.as_deref(), cx);
        let fresh = Tab::new(crate::ui::pane::Pane::leaf(slot));
        if let Some(group) = group {
            *fresh.sidebar_group.borrow_mut() = group;
        }
        let at = at.min(self.tabs.len());
        self.tabs.insert(at, fresh);
        self.maximized = None;
        self.active = at;
        self.focus_active(window, cx);
        self.save_session(cx);
        cx.notify();
    }

    /// Puts a dragged pane down where the last painted frame said it would go.
    ///
    /// Both ends of the drop are named by pane rather than by position, so a
    /// pane that closed between the frame that offered the landing and this one
    /// leaves the drop with nothing to land against, and it is refused.
    fn drop_pane(
        &mut self,
        from: gpui::EntityId,
        zone: crate::ui::pane_drag::DropZone<gpui::EntityId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pane_hover.set(None);
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let leaves = tab.pane.leaves();
        let here = |id| leaves.iter().find(|l| l.entity_id() == id).cloned();
        let (Some(moved), Some(zone)) = (here(from), zone.map(here)) else {
            return;
        };
        if !crate::ui::pane_drag::apply(&mut tab.pane, &moved, zone) {
            return;
        }
        self.maximized = None;
        self.focus_leaf(&moved, window, cx);
        self.save_session(cx);
        cx.notify();
    }

    /// Activates the tab carrying `id`. A workspace this window just switched
    /// to hydrates its tabs asynchronously, so when the tab is not here yet the
    /// request is parked and claimed on the frame it arrives.
    pub(crate) fn activate_tree_tab(
        &mut self,
        id: tty7_core::core::machine::TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.tabs.iter().position(|t| t.tree_id.get() == id) {
            Some(index) => self.activate(index, window, cx),
            None => self.pending_tab = Some(id),
        }
    }

    fn claim_pending_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(want) = self.pending_tab else {
            return;
        };
        let Some(index) = self.tabs.iter().position(|t| t.tree_id.get() == want) else {
            return;
        };
        self.pending_tab = None;
        self.activate(index, window, cx);
    }

    /// Declare to the pane registry which terminals are on screen this
    /// frame: the active tab's, nobody else's. Stated per frame rather than
    /// maintained at every tab operation, the way `scm_sync_watchers` is.
    /// Entity ids only — reading the entities here would put them into the
    /// window's tracked set, which is exactly what the registry routes
    /// around.
    fn declare_displayed_panes(&self, cx: &App) {
        let active = self.active;
        crate::terminal::view::declare_displayed(
            cx,
            self.tabs.iter().enumerate().flat_map(|(i, tab)| {
                tab.pane
                    .leaves()
                    .into_iter()
                    .filter_map(move |slot| Some((slot.terminal()?.entity_id(), i == active)))
            }),
        );
    }

    /// Stamps whichever tab is active right now. Called once per frame rather
    /// than from the ten places that assign `self.active` — it is idempotent,
    /// so the stamp only advances on the first frame after a switch.
    pub(crate) fn touch_active_tab(&self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let top = self.tab_use_seq.get();
        if top != 0 && tab.last_used.get() == top {
            return;
        }
        let next = top + 1;
        self.tab_use_seq.set(next);
        tab.last_used.set(next);
    }

    /// Tab indices most-recently-used first. The active tab always leads, even
    /// before its own stamp lands; tabs never activated trail in strip order.
    pub(crate) fn tabs_by_mru(&self) -> Vec<usize> {
        let stamps: Vec<u64> = self.tabs.iter().map(|t| t.last_used.get()).collect();
        mru_order(&stamps, self.active)
    }

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
            self.remember_active_pane(window, cx);
            // Zoom rides with its tab (#599): stash the outgoing tab's zoom
            // and bring the incoming tab's back. The clears elsewhere (drag,
            // split, close) still stand — those genuinely reshape what was
            // zoomed; merely looking at another tab does not.
            if let Some(outgoing) = self.tabs.get_mut(self.active) {
                outgoing.zoomed = self.maximized.take();
            }
            self.active = index;
            self.maximized = self.tabs[index].zoomed.take().filter(|leaf| {
                // The zoomed pane may have exited while its tab was away —
                // a stale entity must not come back as the zoom.
                self.tabs[index]
                    .pane
                    .leaves()
                    .iter()
                    .any(|l| l.entity_id() == leaf.entity_id())
            });
            self.maybe_refresh_diff_overlay(cx);
            self.sidebar_scroll.scroll_to_item(index);
            if self.code_panel_visible() {
                self.file_tree_refresh_roots(window, cx);
                self.file_tree.focus_handle.focus(window, cx);
            } else {
                self.focus_active(window, cx);
            }
            self.save_session(cx);
            cx.notify();
        }
    }

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
        self.close_tab_inner(index, false, window, cx);
    }

    /// See [`Self::close_pane_inner`] for what `confirmed` carries.
    fn close_tab_inner(
        &mut self,
        index: usize,
        confirmed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.tabs.len() {
            return;
        }
        if !confirmed && let Some(reason) = self.tab_close_reason(index, cx) {
            let id = self.tabs[index].tree_id.get();
            self.ask_before_closing(CloseTarget::Tab(id), reason, window, cx);
            return;
        }
        self.maximized = None;
        let worktree_cwd = self.tab_host_cwd(index, window, cx);
        let snapshot = tab_to_session(&self.tabs[index], cx);
        self.closed.push(snapshot);
        if self.closed.len() > MAX_CLOSED_TABS {
            self.closed.remove(0);
        }
        for leaf in self.tabs[index].pane.terminals() {
            kill_pane_off_thread(leaf.read(cx).pane_route(), leaf.read(cx).pane_id, cx);
        }
        self.tabs.remove(index);
        // Only losing the renaming tab itself ends the rename — closing an
        // unrelated tab must not throw the half-typed name away (#598).
        if self
            .renaming
            .as_ref()
            .is_some_and(|r| !self.tabs.iter().any(|t| t.tree_id.get() == r.tab))
        {
            self.renaming = None;
        }
        if self.tabs.is_empty() {
            self.active = 0;
        } else if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if index < self.active {
            self.active -= 1;
        }
        self.focus_active(window, cx);
        self.save_session(cx);
        cx.notify();
        self.offer_worktree_cleanup(worktree_cwd, cx);
    }

    fn offer_worktree_cleanup(
        &mut self,
        cwd: Option<(crate::ui::host_ops::SharedHost, std::path::PathBuf)>,
        cx: &mut Context<Self>,
    ) {
        let Some((host, cwd)) = cwd else { return };
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
                let path = wt.path.display().to_string();
                let detail = if wt.dirty {
                    t_fmt(L10nKey::AppWorktreeRemoveDetailDirty, &[("path", &path)])
                } else {
                    t_fmt(L10nKey::AppWorktreeRemoveDetailClean, &[("path", &path)])
                };
                let title = t_fmt(L10nKey::AppWorktreeRemoveTitle, &[("branch", &wt.branch)]);
                let level = if wt.dirty {
                    PromptLevel::Warning
                } else {
                    PromptLevel::Info
                };
                let remove_label = if wt.dirty {
                    t(L10nKey::AppWorktreeDiscardAndRemove)
                } else {
                    t(L10nKey::AppWorktreeRemove)
                };
                cx.spawn(async move |this, cx| {
                    let Ok(answer) = this.update_in(cx, |_, window, cx| {
                        window.prompt(
                            level,
                            &title,
                            Some(&detail),
                            &crate::ui::confirm_answers(remove_label, t(L10nKey::AppWorktreeKeep)),
                            cx,
                        )
                    }) else {
                        return;
                    };
                    if !matches!(answer.await, Ok(0)) {
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
                                    t_fmt(L10nKey::AppWorktreeRemoved, &[("branch", &branch)]),
                                    cx,
                                ),
                                Err(e) => window.push_notification(
                                    t_fmt(
                                        L10nKey::AppWorktreeRemoveFailed,
                                        &[("error", &e.to_string())],
                                    ),
                                    cx,
                                ),
                            },
                        );
                    });
                })
                .detach();
            },
        );
    }

    pub(crate) fn close_other_tabs(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.tabs.len() {
            return;
        }
        // A bulk close skips the tabs whose profile asked to be warned about,
        // and closes the rest outright — one dialog per tab is not a question
        // anyone can answer, and only the first would ever get asked. It
        // deliberately does *not* skip merely busy tabs: on a working window
        // that is most of them, and a menu item that quietly closes nothing is
        // worse than one that closes what it says.
        //
        // Unsaved edits are skipped for the SSH reason rather than the busy
        // one, and the difference is what the skip costs. Busy is the state
        // most tabs are in, so skipping it would close nothing; unsaved edits
        // are rare, and they are the one loss here that cannot be undone by
        // doing the thing again.
        for i in (0..self.tabs.len()).rev() {
            if i == index || self.tab_keeps_unclosed_work(i, cx) {
                continue;
            }
            self.close_tab_inner(i, true, window, cx);
        }
    }

    pub(crate) fn close_tabs_right_of(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Same bargain as `close_other_tabs`.
        for i in ((index + 1)..self.tabs.len()).rev() {
            if self.tab_keeps_unclosed_work(i, cx) {
                continue;
            }
            self.close_tab_inner(i, true, window, cx);
        }
    }

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

    /// A tab's working directory, spelled for the clipboard.
    ///
    /// A cwd on this machine is re-spelled with this OS's separators, because
    /// what goes on the clipboard is meant to be pasted into a shell and a
    /// mixed-separator path is not one Windows will take. A remote pane's cwd
    /// keeps the spelling its own machine uses — it is already native over
    /// there. Both "Copy working directory" entry points (the app-menu action
    /// and the tab context menu) come through here so the two cannot drift.
    pub(crate) fn tab_cwd_text(&self, index: usize, window: &Window, cx: &App) -> Option<String> {
        let leaf = self.tabs.get(index)?.pane.focused_or_first(window, cx)?;
        let view = leaf.read(cx);
        let cwd = view.cwd()?;
        Some(match view.local_cwd().is_some() {
            true => crate::ui::path_display::native_separators(&cwd)
                .display()
                .to_string(),
            false => cwd.display().to_string(),
        })
    }

    pub(crate) fn copy_active_cwd(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(text) = self.tab_cwd_text(self.active, window, cx) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

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

        if matches!(placement, ForkPlacement::Split { .. }) {
            self.activate(index, window, cx);
        }

        let (cwd, shell) = {
            let view = source.read(cx);
            (view.local_cwd(), view.shell_spec())
        };
        let group = self.spawn_group(cwd.as_deref(), cx);
        let new = match new_terminal(
            self.window_workspace(cx),
            Some(self.workspace),
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
                window.push_notification(
                    t_fmt(L10nKey::AppOpenTerminalFailed, &[("error", &e.to_string())]),
                    cx,
                );
                return;
            }
        };
        let Some(terminal) = new.terminal() else {
            log::error!("fork spawn produced a pane that is still connecting");
            window.push_notification(t(L10nKey::AppForkStillConnecting), cx);
            return;
        };
        terminal.read(cx).run_command_line(&cmd);

        match placement {
            ForkPlacement::NewTab => {
                self.remember_active_pane(window, cx);
                self.maximized = None;
                let insert_at = self.new_tab_insert_at(cx);
                let tab = Tab::new(Pane::leaf(new));
                if let Some(group) = group {
                    *tab.sidebar_group.borrow_mut() = group;
                }
                self.tabs.insert(insert_at, tab);
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

    pub(crate) fn fork_focused_pane_session(
        &mut self,
        axis: Axis,
        before: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fork_active_pane_session(ForkPlacement::Split { axis, before }, window, cx);
    }

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
            window.push_notification(t(L10nKey::AppPaneNoCodingAgent), cx);
            return None;
        };
        let name = agent.display_name();
        if agent.fork_label().is_none() {
            window.push_notification(t_fmt(L10nKey::AppForkNoCommand, &[("name", name)]), cx);
            return None;
        }
        if remote.is_some() {
            window.push_notification(t_fmt(L10nKey::AppForkLocalOnly, &[("name", name)]), cx);
            return None;
        }
        let session = session.unwrap_or_default();
        let Some(id) = session.session_id.as_deref() else {
            window.push_notification(t_fmt(L10nKey::AppForkNoSessionId, &[("name", name)]), cx);
            return None;
        };
        let Some(cmd) = agent.fork_command(id, session.launch_argv.as_deref()) else {
            window.push_notification(
                t_fmt(L10nKey::AppForkSessionIdNotToken, &[("name", name)]),
                cx,
            );
            return None;
        };
        if session.status == AgentStatus::Working {
            window.push_notification(t_fmt(L10nKey::AppForkMidTurn, &[("name", name)]), cx);
        }
        Some(cmd)
    }

    pub(crate) fn check_for_updates_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        crate::core::update::spawn_check_forced(cx);
        self.open_settings_section(SettingsSection::About, window, cx);
    }

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

    pub(crate) fn tab_is_in_repo(&self, index: usize, window: &Window, cx: &App) -> bool {
        let Some(leaf) = self
            .tabs
            .get(index)
            .and_then(|t| t.pane.focused_or_first(window, cx))
        else {
            return false;
        };
        let view = leaf.read(cx);
        let Some(cwd) = view.git_status_cwd() else {
            return false;
        };
        cx.try_global::<crate::terminal::git_status::GitStatusCache>()
            .and_then(|cache| cache.known_repo_for(view.host_id(), cwd))
            .flatten()
            .is_some()
    }

    pub(crate) fn new_worktree_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((host, cwd)) = self.tab_host_cwd(index, window, cx) else {
            window.push_notification(t(L10nKey::AppTabNoWorkingDirectory), cx);
            return;
        };
        let sheet_host = host.clone();
        let probe_cwd = cwd.clone();
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| crate::core::worktree::defaults(h, &probe_cwd),
            move |this, result, window, cx| match result {
                Ok(defaults) => this.open_worktree_prompt(sheet_host, cwd, defaults, window, cx),
                Err(e) => window.push_notification(
                    t_fmt(L10nKey::AppNewWorktreeFailed, &[("error", &e.to_string())]),
                    cx,
                ),
            },
        );
    }

    pub(crate) fn open_worktree_tab(
        &mut self,
        wt: crate::core::worktree::NewWorktree,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = match new_terminal(
            self.window_workspace(cx),
            Some(self.workspace),
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
                window.push_notification(
                    t_fmt(L10nKey::AppOpenTerminalFailed, &[("error", &e.to_string())]),
                    cx,
                );
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

    pub(crate) fn apply_tab_order(&mut self, order: &[usize], cx: &mut Context<Self>) {
        if order.len() != self.tabs.len() || order.iter().enumerate().all(|(i, &o)| i == o) {
            return;
        }
        // The rename box rides out a reorder: it tracks its tab by tree id,
        // so the drift that once forced it closed here is gone (#598).
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

    /// Opens a rename box on the current name, selected and focused, so the
    /// first thing typed replaces it.
    pub(crate) fn rename_box(
        current: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let input = crate::ui::prefill::filled_box(current, window, cx);
        input.update(cx, |state, cx| state.focus(window, cx));
        input
    }

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
        let input = Self::rename_box(current, window, cx);
        let subs = vec![cx.subscribe_in(
            &input,
            window,
            |this, _input, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } | InputEvent::Blur => this.commit_rename(window, cx),
                _ => {}
            },
        )];
        self.renaming = Some(Renaming {
            tab: self.tabs[index].tree_id.get(),
            input,
            _subs: subs,
        });
        cx.notify();
    }

    pub(crate) fn start_workspace_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current =
            crate::ui::machine_mirror::display_name_for(cx, self.workspace).unwrap_or_default();
        let input = Self::rename_box(current, window, cx);
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

    pub(crate) fn commit_workspace_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rename) = self.workspace_rename.take() else {
            return;
        };
        let value = rename.input.read(cx).value().trim().to_string();
        let id = self.workspace;
        crate::ui::tree_sync::rename_workspace(cx, id, (!value.is_empty()).then_some(value));
        crate::ui::windows::refresh_menu(cx);
        self.sync_window_title(window, cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        let value = renaming.input.read(cx).value().trim().to_string();
        if let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|t| t.tree_id.get() == renaming.tab)
        {
            tab.name = if value.is_empty() { None } else { Some(value) };
        }
        self.save_session(cx);
        crate::ui::windows::refresh_menu(cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    pub(crate) fn palette_commands(&self, window: &Window, cx: &App) -> Vec<Command> {
        let mut commands = Command::base_commands(
            cx,
            ChromeState {
                rail_collapsed: self.sidebar_collapsed,
                right_panel_visible: self.right_panel_visible,
                document_filled: self.document_layout(cx)
                    == crate::core::config::DocumentLayout::Fill,
            },
        );

        // Offered only where it would do something. A connection opened from a
        // saved host has nothing to save, and a pane that is not an SSH one has
        // no connection at all — either would be a row that quietly did nothing
        // (#549).
        if self.unsaved_ssh_session(window, cx).is_some() {
            commands.push(
                Command::localized(
                    L10nKey::CmdSshSaveConnection,
                    CommandKind::SaveSshSessionAsHost,
                )
                .with_subtitle(t(L10nKey::CmdSshSaveConnectionSubtitle))
                .in_group(CommandGroup::Ssh),
            );
        }

        for p in crate::ui::ssh_connect::ssh_profiles_by_frecency(cx) {
            let subtitle = crate::core::ssh_profile::to_connect_string(&p);
            let title = if p.name.is_empty() {
                subtitle.clone()
            } else {
                p.name.clone()
            };
            commands.push(
                Command::new(
                    t_fmt(L10nKey::AppCmdSshProfileTitle, &[("title", &title)]),
                    CommandKind::ConnectSavedProfile(p.id),
                )
                .with_subtitle(subtitle)
                .in_group(CommandGroup::Ssh),
            );
        }

        for (i, tab) in self.tabs.iter().enumerate() {
            if i == self.active {
                continue;
            }
            let label = self.tab_label(tab, i, None, cx);
            commands.push(
                Command::new(
                    t_fmt(L10nKey::AppCmdSwitchToTab, &[("label", &label)]),
                    CommandKind::ActivateTab(i),
                )
                .in_group(CommandGroup::TabsPanes),
            );
        }
        commands
    }

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        self.open_palette("", window, cx);
    }

    /// Open the palette with `query` already in its search field.
    ///
    /// Unlike [`Self::toggle_palette`] this always opens. A row that names
    /// what it will show cannot also be the thing that dismisses it, and the
    /// only ways in here are rows like that.
    pub(crate) fn open_palette(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let commands = self.palette_commands(window, cx);
        let view = cx.new(|cx| PaletteView::seeded(commands, query, window, cx));
        self.palette_sub = Some(cx.subscribe_in(&view, window, Self::on_palette_event));
        self.palette = Some(view);
        cx.notify();
    }

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
                // The picker is already showing this theme; keep it through the
                // close instead of reverting and re-applying it.
                if matches!(kind, CommandKind::SetTheme(_)) {
                    self.theme_preview_restore = None;
                }
                self.close_palette(window, cx);
                self.run_command(kind, window, cx);
            }
            PaletteEvent::Dismiss => self.close_palette(window, cx),
            PaletteEvent::PreviewTheme(i) => {
                if let Some(id) = crate::ui::presets::all(cx).get(*i).map(|t| t.id.clone()) {
                    self.preview_preset(&id, window, cx);
                }
            }
            PaletteEvent::CancelThemePreview => self.cancel_preset_preview(window, cx),
        }
    }

    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        self.palette_sub = None;
        // A previewed theme was never persisted: closing the palette any way
        // other than confirming the pick puts the old one back.
        self.cancel_preset_preview(window, cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    fn focused_leaf(&self, window: &Window, cx: &App) -> Option<Entity<TerminalView>> {
        self.tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
    }

    fn bump_command_frecency(&mut self, kind: &CommandKind, cx: &mut Context<Self>) {
        let Some(id) = kind.id() else { return };
        self.update_config(cx, |cfg| {
            let entry = cfg.command_frecency.entry(id.to_string()).or_default();
            entry.count = entry.count.saturating_add(1);
            entry.last_used = crate::core::config::unix_now();
        });
    }

    fn run_command(&mut self, kind: CommandKind, window: &mut Window, cx: &mut Context<Self>) {
        use CommandKind::*;
        self.bump_command_frecency(&kind, cx);
        match kind {
            NewTab => self.new_tab(window, cx),
            NewWorkspace => self.open_workspace_form(window, cx),
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
            // Same exit as the tray's: confirm, then stop the server with the
            // app. A bare quit would leave the daemon orphaned behind a dead
            // tray icon.
            Quit => self.quit_stop_sessions(window, cx),
            RestartDaemon => self.restart_window_daemon(window, cx),
            ToggleSftp => self.toggle_sftp(window, cx),
            ShowSshForwards => self.show_ssh_forwards(window, cx),
            ToggleCodePanel => self.toggle_code_panel(window, cx),
            ToggleDocumentFill => self.toggle_document_fill(cx),
            DocumentWidthThird => {
                self.set_document_ratio(crate::core::config::DOCUMENT_RATIO_THIRD, cx)
            }
            DocumentWidthHalf => {
                self.set_document_ratio(crate::core::config::DOCUMENT_RATIO_HALF, cx)
            }
            DocumentWidthTwoThirds => {
                self.set_document_ratio(crate::core::config::DOCUMENT_RATIO_TWO_THIRDS, cx)
            }
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
            SaveSshSessionAsHost => self.save_ssh_session_as_host(window, cx),
            OpenSshProfiles => self.open_settings_section(SettingsSection::Ssh, window, cx),
            SendSelectionToAgent => self.send_selection_to_agent(window, cx),
            SendGitDiffToAgent => self.send_git_diff_to_agent(window, cx),
            ScmCommit => self.run_scm_action(ScmIntent::Commit, window, cx),
            ScmStageAll => self.run_scm_action(ScmIntent::StageAll, window, cx),
            ScmUnstageAll => self.run_scm_action(ScmIntent::UnstageAll, window, cx),
            ScmDiscardAll => self.run_scm_action(ScmIntent::DiscardAll, window, cx),
            ScmPush => self.run_scm_action(ScmIntent::Push, window, cx),
            ScmCommitAmend => self.run_scm_action(ScmIntent::CommitAmend, window, cx),
            ScmRefresh => self.run_scm_action(ScmIntent::Refresh, window, cx),
            ScmToggleGraph => self.scm_toggle_graph(cx),
            ScmPull => self.run_scm_action(ScmIntent::Pull, window, cx),
            ScmFetch => self.run_scm_action(ScmIntent::Fetch, window, cx),
            ScmSync => self.run_scm_action(ScmIntent::Sync, window, cx),
            ScmCreateBranch => self.run_scm_action(ScmIntent::CreateBranch, window, cx),
            OpenBranchPicker => self.run_scm_action(ScmIntent::CheckoutBranch, window, cx),
            ToggleDiffViewMode => self.toggle_diff_view_mode(cx),
            OpenThemePicker | OpenSshConnectInput => {}
            ActivateTab(i) => self.activate(i, window, cx),
        }
    }

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

    fn deliver_agent_prompt(&mut self, prompt: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.agent_target_leaf(cx) else {
            crate::terminal::notify_desktop(Some("tty7"), t(L10nKey::AppNoRunningCodingAgent));
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
            crate::terminal::notify_desktop(Some("tty7"), t(L10nKey::AppNothingSelected));
            return;
        };
        let cwd = cwd.map(|c| c.to_string_lossy().into_owned());
        if let Some(prompt) =
            crate::core::agent_prompt::build_selection_prompt(&selection, cwd.as_deref())
        {
            self.deliver_agent_prompt(&prompt, window, cx);
        }
    }

    fn send_git_diff_to_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pane = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx));
        let target = pane.and_then(|view| {
            let view = view.read(cx);
            Some((view.host(cx)?, view.host_cwd()?))
        });
        let Some((host, cwd)) = target else {
            crate::terminal::notify_desktop(Some("tty7"), t(L10nKey::AppPaneNoKnownDirectory));
            return;
        };
        crate::ui::host_ops::HostOps::run_in(
            host,
            window,
            cx,
            move |h| {
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
                        &t_fmt(L10nKey::AppNoUncommittedChanges, &[("cwd", &cwd_s)]),
                    ),
                }
            },
        );
    }

    fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.is_some() {
            self.close_settings_checked(window, cx);
            return;
        }
        self.remember_active_pane(window, cx);
        let focus_handle = cx.focus_handle();
        let mut subs = Vec::new();
        let (font_select, font_bold_select, font_italic_select) =
            self.build_font_selects(&mut subs, window, cx);
        let language_select = self.build_language_select(&mut subs, window, cx);
        #[cfg(target_os = "windows")]
        let window_backdrop_select = self.build_window_backdrop_select(&mut subs, window, cx);
        let (shell_program_input, shell_args_input, wd_path_input) =
            self.build_shell_inputs(&mut subs, window, cx);
        let link_file_command_input = self.build_link_file_command_input(&mut subs, window, cx);
        let http_proxy_input = self.build_http_proxy_input(&mut subs, window, cx);
        let scroll_slider = self.build_scroll_slider(&mut subs, window, cx);
        let window_opacity_slider = self.build_window_opacity_slider(&mut subs, window, cx);
        let theme_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t(crate::ui::i18n::L10nKey::SearchThemes))
        });
        subs.push(
            cx.subscribe_in(&theme_search, window, |_this, _i, ev, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        );
        let settings_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t(crate::ui::i18n::L10nKey::SearchSettings))
        });
        subs.push(
            cx.subscribe_in(&settings_search, window, |this, _i, ev, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.autoselect_settings_search(cx);
                    if let Some(s) = this.active_settings_mut() {
                        s.reveal_first_hit.set(true);
                    }
                    cx.notify();
                }
            }),
        );

        let ssh_filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t(crate::ui::i18n::L10nKey::FilterHosts))
        });
        subs.push(
            cx.subscribe_in(&ssh_filter, window, |_this, _i, ev, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        );

        let ssh_quick_connect = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t(L10nKey::AppPlaceholderSshQuickConnect))
        });
        subs.push(
            cx.subscribe_in(&ssh_quick_connect, window, |_this, _i, ev, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        );

        let content_scroll = gpui::ScrollHandle::new();
        let search_anchor = gpui::ScrollAnchor::for_handle(content_scroll.clone());

        self.settings = Some(SettingsState {
            focus_handle: focus_handle.clone(),
            section: SettingsSection::Appearance,
            search: settings_search,
            content_scroll,
            ssh_master_scroll: gpui::ScrollHandle::new(),
            ssh_detail_scroll: gpui::ScrollHandle::new(),
            theme_list_scroll: gpui::ScrollHandle::new(),
            search_anchor,
            reveal_first_hit: Cell::new(false),
            font_select,
            font_bold_select,
            font_italic_select,
            language_select,
            #[cfg(target_os = "windows")]
            window_backdrop_select,
            shell_program_input,
            shell_args_input,
            wd_path_input,
            link_file_command_input,
            http_proxy_input,
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
        let search_focus = self
            .settings
            .as_ref()
            .map(|s| s.search.read(cx).focus_handle(cx));
        match search_focus {
            Some(handle) => window.focus(&handle, cx),
            None => window.focus(&focus_handle, cx),
        }
        self.rebuild_theme_editor(window, cx);
        self.ensure_agent_hooks_loaded(cx);
        cx.notify();
    }

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
        let build_alt_font_select = |value: &Option<String>,
                                     names: &[String],
                                     window: &mut Window,
                                     cx: &mut Context<Self>| {
            let mut rows = Vec::with_capacity(names.len() + 1);
            rows.push(crate::ui::settings::font_default_label().to_string());
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

    fn build_language_select(
        &mut self,
        subs: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SelectState<SearchableVec<String>>> {
        let labels = || {
            crate::ui::i18n::SUPPORTED_LANGUAGES
                .iter()
                .map(|lang| t(lang.label_key).to_string())
                .collect::<Vec<_>>()
        };
        let cfg = cx.global::<Config>();
        let current = Self::normalize_gui_language(&cfg.gui_language);
        let rows = labels();
        let selected = crate::ui::i18n::SUPPORTED_LANGUAGES
            .iter()
            .position(|lang| lang.code == current)
            .unwrap_or(0);
        let language_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(rows),
                Some(IndexPath::default().row(selected)),
                window,
                cx,
            )
        });
        subs.push(cx.subscribe_in(
            &language_select,
            window,
            move |this, _select, ev: &SelectEvent<SearchableVec<String>>, window, cx| {
                if let SelectEvent::Confirm(Some(label)) = ev {
                    let rows = labels();
                    if let Some(idx) = rows.iter().position(|r| r == label)
                        && let Some(lang) = crate::ui::i18n::SUPPORTED_LANGUAGES.get(idx)
                    {
                        this.set_gui_language(lang.code, window, cx);
                    }
                }
            },
        ));
        language_select
    }

    fn normalize_gui_language(code: &str) -> &'static str {
        crate::ui::i18n::find_language(code)
            .map(|lang| lang.code)
            .unwrap_or_else(crate::ui::i18n::default_language_code)
    }

    /// The backdrop dropdown only lists the presets this Windows build
    /// supports, in the order of `theme::supported_backdrops`; the select
    /// resolves the picked label back through that same list.
    #[cfg(target_os = "windows")]
    fn build_window_backdrop_select(
        &mut self,
        subs: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SelectState<SearchableVec<String>>> {
        let rows = window_backdrop_labels(cx.global::<Config>().window_backdrop);
        let selected = window_backdrop_index(cx.global::<Config>().window_backdrop);
        let select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(rows),
                Some(IndexPath::default().row(selected)),
                window,
                cx,
            )
        });
        subs.push(cx.subscribe_in(
            &select,
            window,
            move |this, _select, ev: &SelectEvent<SearchableVec<String>>, window, cx| {
                if let SelectEvent::Confirm(Some(label)) = ev {
                    let current = cx.global::<Config>().window_backdrop;
                    let rows = window_backdrop_labels(current);
                    if let Some(idx) = rows.iter().position(|row| row == label) {
                        this.set_window_backdrop(
                            window_backdrop_from_index(idx, current),
                            window,
                            cx,
                        );
                    }
                }
            },
        ));
        select
    }

    pub(crate) fn set_gui_language(
        &mut self,
        code: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let code = Self::normalize_gui_language(code);
        {
            let cfg = cx.global_mut::<Config>();
            cfg.gui_language = code.to_string();
        }
        set_locale(code);
        cx.global::<Config>().save();
        set_menus(cx);
        // Explorer reads its menu wording from the registry, so it is the one
        // surface a language change does not reach on its own. No-op unless
        // the user installed the context menu, and off Windows entirely.
        crate::core::explorer_context_menu::refresh_labels();
        self.refresh_locale_state(window, cx);
        crate::ui::windows::WindowRegistry::refresh_locale(cx, Some(self.workspace));
    }

    pub(crate) fn refresh_locale_state(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_search.update(cx, |state, cx| {
            state.set_placeholder(t(L10nKey::SearchTabs), window, cx)
        });
        self.file_search.update(cx, |state, cx| {
            state.set_placeholder(t(L10nKey::SearchFiles), window, cx)
        });
        // The remote Files panel is built once with the app, so its placeholder
        // is the one input that would otherwise keep the old language.
        self.sftp_panel.filter_input.update(cx, |state, cx| {
            state.set_placeholder(t(L10nKey::SearchFiles), window, cx)
        });
        if let Some(s) = self.active_settings() {
            let rows = crate::ui::i18n::SUPPORTED_LANGUAGES
                .iter()
                .map(|lang| t(lang.label_key).to_string())
                .collect::<Vec<_>>();
            s.language_select.update(cx, |state, cx| {
                state.set_items(SearchableVec::new(rows), window, cx);
                let code = Self::normalize_gui_language(&cx.global::<Config>().gui_language);
                let selected = crate::ui::i18n::SUPPORTED_LANGUAGES
                    .iter()
                    .position(|lang| lang.code == code)
                    .unwrap_or(0);
                state.set_selected_index(Some(IndexPath::default().row(selected)), window, cx);
            });
            #[cfg(target_os = "windows")]
            s.window_backdrop_select.update(cx, |state, cx| {
                let current = cx.global::<Config>().window_backdrop;
                let rows = window_backdrop_labels(current);
                state.set_items(SearchableVec::new(rows), window, cx);
                // `set_items` does not preserve the selection; restore the
                // index of the stored value so a locale refresh (which
                // re-translates the labels) cannot leave the dropdown
                // showing no — or the wrong — selection.
                state.set_selected_index(
                    Some(IndexPath::default().row(window_backdrop_index(current))),
                    window,
                    cx,
                );
            });
            s.search.update(cx, |state, cx| {
                state.set_placeholder(t(L10nKey::SearchSettings), window, cx)
            });
            s.theme_search.update(cx, |state, cx| {
                state.set_placeholder(t(L10nKey::SearchThemes), window, cx)
            });
            s.ssh_filter.update(cx, |state, cx| {
                state.set_placeholder(t(L10nKey::FilterHosts), window, cx)
            });
            s.ssh_quick_connect.update(cx, |state, cx| {
                state.set_placeholder(t(L10nKey::AppPlaceholderSshQuickConnect), window, cx)
            });
            s.shell_args_input.update(cx, |state, cx| {
                state.set_placeholder(t(L10nKey::AppPlaceholderNone), window, cx)
            });
            if !cfg!(windows) {
                s.shell_program_input.update(cx, |state, cx| {
                    state.set_placeholder(t(L10nKey::AppPlaceholderLoginShell), window, cx)
                });
            }
            s.link_file_command_input.update(cx, |state, cx| {
                state.set_placeholder(t(L10nKey::AppPlaceholderOpenInDefaultApp), window, cx)
            });
        }
        cx.notify();
    }

    fn build_shell_inputs(
        &mut self,
        subs: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<InputState>, Entity<InputState>, Entity<InputState>) {
        let cfg = cx.global::<Config>();
        let (shell_program, shell_args) = match &cfg.shell {
            // Quote each argument back into field text rather than joining on
            // spaces: `join(" ")` cannot spell an argument that contains one,
            // so a perfectly legal `"args": ["-c", "echo hi"]` in config.json
            // refilled as three words and re-committed as three argv the moment
            // the field lost focus — the user never typed a thing (#551).
            // `join_shell_args` quotes only what needs it, and `commit_shell`'s
            // matching `split_shell_args` parses it back losslessly.
            Some(s) => (s.program.clone(), join_shell_args(&s.args)),
            None => (String::new(), String::new()),
        };
        let wd_path = cfg.working_directory.path.clone();
        let platform_default = if cfg!(windows) {
            "PowerShell"
        } else {
            t(L10nKey::AppPlaceholderLoginShell)
        };
        let shell_program_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(platform_default)
                .default_value(shell_program)
        });
        let shell_args_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t(L10nKey::AppPlaceholderNone))
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
                .placeholder(t(L10nKey::AppPlaceholderOpenInDefaultApp))
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

    fn build_http_proxy_input(
        &mut self,
        subs: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let value = cx.global::<Config>().http_proxy.clone().unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("http://127.0.0.1:7890")
                .default_value(value)
        });
        subs.push(
            cx.subscribe_in(&input, window, move |this, _i, ev, _w, cx| {
                if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.commit_http_proxy(cx);
                }
            }),
        );
        input
    }

    fn commit_http_proxy(&mut self, cx: &mut Context<Self>) {
        let Some(value) = self
            .active_settings()
            .map(|s| s.http_proxy_input.read(cx).value().trim().to_string())
        else {
            return;
        };
        // Keep an unusable value out of `config.json`. The row renders a hint
        // under the input, so the typo does not silently vanish either.
        if !value.is_empty() && !tty7_core::daemon::install::proxy::is_valid_manual(&value) {
            cx.notify();
            return;
        }
        let value = (!value.is_empty()).then_some(value);
        let cfg = cx.global_mut::<Config>();
        if cfg.http_proxy == value {
            return;
        }
        cfg.http_proxy = value;
        cfg.save();
        cx.notify();
    }

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
            return;
        }
        cfg.link_file_command = command;
        cfg.save();
        cx.notify();
    }

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

    fn build_scroll_slider(
        &mut self,
        subs: &mut Vec<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SliderState> {
        let scroll_mult = cx.global::<Config>().mouse_scroll_multiplier;
        // Ends read from the clamp, not written again here: the slider used to
        // span 0.5..=5.0 against a clamp of 0.1..=10, so the documented ends
        // were unreachable from the window and a hand-set 8x sat under a thumb
        // pinned at 5. The step is what the wider span costs — a tenth, so 1.00
        // is still a position rather than something to be landed on between two.
        let scroll_slider = cx.new(|_| {
            SliderState::new()
                .min(crate::core::config::MOUSE_SCROLL_MULTIPLIER_MIN)
                .max(crate::core::config::MOUSE_SCROLL_MULTIPLIER_MAX)
                .step(0.1)
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

    pub(crate) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

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

    fn commit_font_family_emphasis(&mut self, bold: bool, name: String, cx: &mut Context<Self>) {
        let family = (name != crate::ui::settings::font_default_label()).then_some(name);
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

    fn reload_from_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        apply_theme(Some(window), cx);
        self.sync_window_opacity_slider(window, cx);
        // Another window — or a hand edit / config sync picked up by the
        // `Config` watcher — can change the backdrop while this window's
        // settings panel is open. The window itself already switched
        // material above, so the dropdown has to follow or it contradicts
        // what it describes.
        #[cfg(target_os = "windows")]
        self.sync_window_backdrop_select(window, cx);
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
        self.sidebar_width.set(cx.global::<Config>().sidebar_width);
        self.right_panel_width
            .set(cx.global::<Config>().right_panel_width);
        self.document_ratio
            .set(cx.global::<Config>().document_ratio);
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
        let report_mouse = cx.global::<Config>().mouse_reporting;
        let prompt_editor = cx.global::<Config>().prompt_editor;
        for tab in &self.tabs {
            for leaf in tab.pane.terminals() {
                leaf.update(cx, |v, cx| {
                    if v.report_mouse != report_mouse {
                        v.report_mouse = report_mouse;
                        cx.notify();
                    }
                    // A hand edit of `config.json` — or another window's
                    // settings page — has to move a live pane between the
                    // local editor and ZLE too, not just the window that
                    // flipped the switch.
                    v.set_prompt_editor(prompt_editor, cx);
                });
            }
        }
        cx.notify();
    }

    /// Saves the shell after the detected-shell menu wrote into the field.
    ///
    /// The field itself only commits on Enter or blur, and picking from a menu
    /// is neither — without this the choice would sit in the box unsaved until
    /// the user happened to click into it and out again.
    pub(crate) fn commit_shell_from_picker(&mut self, cx: &mut Context<Self>) {
        self.commit_shell(cx);
        cx.notify();
    }

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
        // Split the way a command line splits, not on raw whitespace: the text
        // goes on to become argv verbatim, and `split_whitespace` kept the
        // quote characters while tearing the quoted string apart — `-c "echo
        // hi"` reached the shell as `["-c", "\"echo", "hi\""]` (#551).
        let raw_args = settings.shell_args_input.read(cx).value();
        let args: Vec<String> = match split_shell_args(&raw_args) {
            Ok(args) => args,
            // A quote that never closes names no argv at all, so this text
            // cannot go into `config.json` — the proxy row's pattern: leave the
            // stored arguments alone and let the row explain why under the
            // input after this notify. The rest of the row still commits: the
            // Program the user just typed, or picked from the menu, is not
            // theirs to lose over the field below it.
            Err(_) => {
                cx.notify();
                let cfg = cx.global::<Config>();
                cfg.shell
                    .as_ref()
                    .map(|s| s.args.clone())
                    .unwrap_or_default()
            }
        };
        let shell = if program.is_empty() {
            None
        } else {
            Some(ShellConfig { program, args })
        };
        {
            let cfg = cx.global_mut::<Config>();
            if cfg.shell == shell {
                return;
            }
            cfg.shell = shell;
            cfg.save();
        }
        // Shell discovery runs off the UI thread and now includes the saved
        // configured shell, so refresh the menu without blocking Settings.
        self.refresh_shells(cx);
    }

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

    fn commit_working_directory_path(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self
            .active_settings()
            .map(|s| s.wd_path_input.read(cx).value().trim().to_string())
        else {
            return;
        };
        // A typo here is not a directory, and the daemon then silently falls
        // back to its own cwd for every new pane — "new shells don't start in
        // my project" reads as a tty7 bug rather than a typo (#601). Refuse
        // to save, the proxy row's pattern (#551): the field keeps the text,
        // the settings row explains in red, and the last good value stays in
        // config.json.
        if !wd_path_saveable(&path) {
            cx.notify();
            return;
        }
        let cfg = cx.global_mut::<Config>();
        if cfg.working_directory.path == path {
            return;
        }
        cfg.working_directory.path = path;
        cfg.save();
        cx.notify();
    }

    pub(crate) fn active_settings(&self) -> Option<&SettingsState> {
        self.settings.as_ref()
    }

    pub(crate) fn active_settings_mut(&mut self) -> Option<&mut SettingsState> {
        self.settings.as_mut()
    }

    pub(crate) fn tab_ssh_dot(&self, tab: &Tab, cx: &App) -> Option<u32> {
        use crate::daemon::protocol::SshPhase;
        let leaf = tab.pane.first_leaf()?;
        let v = leaf.terminal()?.read(cx);
        if let Some(phase) = v.ssh_phase() {
            let rgb = if v.ssh_disconnected() {
                0xEF4444
            } else {
                match phase {
                    SshPhase::Connecting | SshPhase::Authenticating => 0xF59E0B,
                    SshPhase::Connected => 0x22C55E,
                    SshPhase::Failed { .. } => 0xEF4444,
                }
            };
            Some(rgb)
        } else if v
            .remote_context()
            .is_some_and(|r| r.kind != crate::daemon::protocol::RemoteKind::Wsl)
        {
            Some(0x9CA3AF)
        } else {
            None
        }
    }

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

    /// The app asks every other question of this class through the platform's
    /// own dialog. This one used to be a bespoke in-app card with no scrim, no
    /// Escape and no click-outside — the two buttons were the only way out.
    fn ask_before_closing(
        &mut self,
        target: CloseTarget,
        reason: CloseReason,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_prompt_open {
            return;
        }
        self.close_prompt_open = true;
        // ⌘W closes a pane, but when it is the only one in its tab the tab goes
        // with it — the question has to name what actually disappears.
        let ends_the_tab = matches!(target, CloseTarget::Tab(_))
            || self
                .tabs
                .get(self.active)
                .is_some_and(|tab| tab.pane.leaves().len() <= 1);
        let (title, body) = close_prompt(ends_the_tab, &reason);
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(&body),
            &crate::ui::confirm_answers(
                t(crate::ui::i18n::L10nKey::Close),
                t(crate::ui::i18n::L10nKey::Keep),
            ),
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let close = matches!(answer.await, Ok(0));
            let _ = this.update_in(cx, |this, window, cx| {
                this.close_prompt_open = false;
                // Cancelled, or the window went away with the question open:
                // either way the work carries on.
                if !close {
                    cx.notify();
                    return;
                }
                match target {
                    // The tab may have moved, or gone, while the question was
                    // up; find it by id and let it be if it is already closed.
                    CloseTarget::Tab(id) => {
                        if let Some(i) = this.tabs.iter().position(|t| t.tree_id.get() == id) {
                            this.close_tab_inner(i, true, window, cx);
                        }
                    }
                    CloseTarget::Pane => this.close_pane_inner(true, window, cx),
                }
            });
        })
        .detach();
    }

    /// The first reason this pane should not simply vanish.
    fn leaf_close_reason(&self, leaf: &Entity<TerminalView>, cx: &App) -> Option<CloseReason> {
        if self.leaf_is_warn_ssh(leaf, cx) {
            return Some(CloseReason::LiveSsh);
        }
        leaf.read(cx).busy().map(CloseReason::Busy)
    }

    /// Whether closing this tab would drop a connection the user asked to be
    /// warned about. Narrower than [`Self::tab_close_reason`] on purpose — see
    /// the bulk closes, which skip these and only these.
    /// What a bulk close leaves standing: the two losses worth refusing to
    /// take without being asked, where a busy command is not one.
    fn tab_keeps_unclosed_work(&self, index: usize, cx: &App) -> bool {
        self.tab_has_warn_ssh(index, cx) || self.tab_unsaved_edit(index).is_some()
    }

    fn tab_has_warn_ssh(&self, index: usize, cx: &App) -> bool {
        self.tabs.get(index).is_some_and(|tab| {
            tab.pane
                .terminals()
                .iter()
                .any(|l| self.leaf_is_warn_ssh(l, cx))
        })
    }

    /// What closing the whole window would lose without asking, if anything.
    ///
    /// Only unwritten buffers. A busy command and a live SSH link are the
    /// daemon's and survive the window going away — that is the whole point
    /// of closing being the keep-everything exit — so warning about them here
    /// would be warning about nothing. Text that was never written down is
    /// the one thing in the window that has nowhere else to be.
    pub(crate) fn unsaved_edit_to_confirm(&self) -> Option<(usize, String)> {
        if self.close_confirmed {
            return None;
        }
        self.unsaved_edit_in_window()
    }

    #[cfg(test)]
    pub(crate) fn confirm_window_close_for_test(&mut self) {
        self.close_confirmed = true;
    }

    /// Asks about the buffers, then closes the window for real if told to.
    fn confirm_window_close(
        &mut self,
        tab: usize,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Shown first, so the answer is about a tab the user can see rather
        // than a file name they have to go looking for.
        self.activate(tab, window, cx);
        let answer = window.prompt(
            PromptLevel::Warning,
            t(L10nKey::CloseWindowUnsavedEditsTitle),
            Some(&t_fmt(L10nKey::CloseUnsavedEditsBody, &[("name", &name)])),
            &crate::ui::confirm_answers(t(L10nKey::Close), t(L10nKey::Keep)),
            cx,
        );
        cx.spawn_in(window, async move |app, cx| {
            let Ok(0) = answer.await else { return };
            let _ = app.update_in(cx, |app, window, _cx| {
                app.close_confirmed = true;
                window.remove_window();
            });
        })
        .detach();
    }

    /// Unsaved edits come first, because they are the only loss here that
    /// cannot be undone by doing the thing again.
    fn tab_close_reason(&self, index: usize, cx: &App) -> Option<CloseReason> {
        if let Some(name) = self.tab_unsaved_edit(index) {
            return Some(CloseReason::UnsavedEdits(name));
        }
        self.tabs
            .get(index)?
            .pane
            .terminals()
            .iter()
            .find_map(|l| self.leaf_close_reason(l, cx))
    }

    fn focused_pane_close_reason(&self, window: &Window, cx: &App) -> Option<CloseReason> {
        let tab = self.tabs.get(self.active)?;
        // Closing the last pane takes the tab, and the tab's code panel with
        // it. `close_pane_inner` carries its own `confirmed` into
        // `close_tab_inner` so the tab does not ask a second question, which
        // means a question the pane never asked is never asked at all.
        if tab.pane.leaves().len() <= 1
            && let Some(name) = self.tab_unsaved_edit(self.active)
        {
            return Some(CloseReason::UnsavedEdits(name));
        }
        let leaf = tab.pane.focused_or_first(window, cx)?;
        self.leaf_close_reason(&leaf, cx)
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
        let remote = pane.remote_context()?;
        (remote.kind != crate::daemon::protocol::RemoteKind::Wsl).then_some((pane.pane_id, remote))
    }

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

    pub(crate) fn select_settings_section(
        &mut self,
        target: SettingsSection,
        cx: &mut Context<Self>,
    ) {
        if let Some(s) = self.settings.as_mut() {
            let moved = s.section != target;
            s.section = target;
            s.recording = None;
            // Arriving on a page with a query live means arriving to look for
            // what the nav badge counted, so take the page to it.
            if moved {
                s.reveal_first_hit.set(true);
            }
            if target == SettingsSection::Agents {
                s.agent_hooks_states = crate::ui::settings::AgentHooksView::Loading;
            }
        }
        self.ensure_agent_hooks_loaded(cx);
        cx.notify();
    }

    fn ensure_agent_hooks_loaded(&mut self, cx: &mut Context<Self>) {
        if self
            .active_settings()
            .is_some_and(|s| s.section == SettingsSection::Agents)
        {
            self.load_agent_hooks_states(cx);
        }
    }

    pub(crate) fn agent_hooks_machines(
        &self,
        cx: &mut App,
    ) -> Vec<crate::ui::settings::AgentHooksMachine> {
        use crate::ui::settings::AgentHooksMachine;
        let mut out = vec![AgentHooksMachine {
            host: crate::ui::host_ops::HostId::LOCAL,
            label: t(L10nKey::AppAgentHooksThisComputer).to_string(),
        }];
        let configured = crate::ui::remote_connect::available_hosts(cx);
        for id in crate::ui::host_registry::HostRegistry::ids(cx) {
            if id.is_local() {
                continue;
            }
            let label = configured
                .iter()
                .find(|h| h.target.host_id() == id)
                .map(|h| h.label.clone())
                .unwrap_or_else(|| t(L10nKey::AppAgentHooksRemoteMachine).to_string());
            out.push(AgentHooksMachine { host: id, label });
        }
        out
    }

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
            s.agent_hooks_note = None;
            s.agent_hooks_states = crate::ui::settings::AgentHooksView::Loading;
        }
        self.load_agent_hooks_states(cx);
        cx.notify();
    }

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
                s.agent_hooks_states = AgentHooksView::Unavailable(Self::agent_hooks_offline_msg());
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
                            t(L10nKey::AppAgentHooksNoHomeDir).to_string(),
                        ),
                    };
                    cx.notify();
                }
            },
        );
    }

    fn agent_hooks_offline_msg() -> String {
        t(L10nKey::AppAgentHooksOffline).to_string()
    }

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
        let home = crate::ui::remote_connect::HostLinks::home(cx, host_id)?;
        Some((host, Some(home)))
    }

    pub(crate) fn settings_install_agent_hooks(
        &mut self,
        agent: crate::core::agent_hooks::HookAgent,
        cx: &mut Context<Self>,
    ) {
        self.run_agent_hooks_action(agent, true, cx);
    }

    pub(crate) fn settings_uninstall_agent_hooks(
        &mut self,
        agent: crate::core::agent_hooks::HookAgent,
        cx: &mut Context<Self>,
    ) {
        self.run_agent_hooks_action(agent, false, cx);
    }

    /// Words a hook install or removal for the note in Settings.
    ///
    /// `agent_hooks` returns what it did rather than a sentence, because it
    /// lives in `tty7-core` and cannot reach `src/ui/i18n` — it used to hand
    /// back English prose, which a Chinese or Japanese UI then showed as-is.
    fn agent_hooks_outcome_msg(outcome: &crate::core::agent_hooks::HookOutcome) -> String {
        use crate::core::agent_hooks::HookOutcome as O;
        match outcome {
            O::Installed => t(L10nKey::AppAgentHooksInstalled).to_string(),
            O::InstalledEnableCodexThere => {
                t(L10nKey::AppAgentHooksInstalledEnableCodexThere).to_string()
            }
            O::InstalledCodexEnableFailed(e) => t_fmt(
                L10nKey::AppAgentHooksInstalledCodexEnableFailed,
                &[("error", e)],
            ),
            O::Removed => t(L10nKey::AppAgentHooksRemoved).to_string(),
            O::NothingInstalled => t(L10nKey::AppAgentHooksNothingInstalled).to_string(),
            O::NoTty7Hooks => t(L10nKey::AppAgentHooksNoTty7Hooks).to_string(),
        }
    }

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
                s.agent_hooks_note = Some((agent, Self::agent_hooks_offline_msg()));
                s.agent_hooks_states = crate::ui::settings::AgentHooksView::Unavailable(
                    Self::agent_hooks_offline_msg(),
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
                    None => HookTarget::local(h).ok_or_else(|| {
                        anyhow::anyhow!("{}", t(L10nKey::AppAgentHooksHomeDirUnresolved))
                    })?,
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
                            Ok(outcome) => Self::agent_hooks_outcome_msg(&outcome),
                            // Every sibling error names its action; this one
                            // said only "Failed:", leaving the note that
                            // reports it silent about which half of the
                            // toggle had not happened.
                            Err(e) => t_fmt(
                                match install {
                                    true => L10nKey::AppAgentHooksInstallFailed,
                                    false => L10nKey::AppAgentHooksRemoveFailed,
                                },
                                &[("error", &e.to_string())],
                            ),
                        },
                    ));
                }
                this.load_agent_hooks_states(cx);
                cx.notify();
            },
        );
    }

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

    pub(crate) fn start_recording_key(
        &mut self,
        action: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.weak_entity();
        let intercept = cx.intercept_keystrokes(move |ev, _window, cx| {
            let keystroke = ev.keystroke.clone();
            let _ = this.update(cx, |this, cx| this.on_record_key(&keystroke, cx));
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

    fn stop_recording(&mut self, cx: &mut Context<Self>) {
        self.record_gen = self.record_gen.wrapping_add(1);
        if let Some(s) = self.active_settings_mut() {
            s.recording = None;
        }
        cx.notify();
    }

    fn assign_keybinding(&mut self, action: String, spec: String, cx: &mut Context<Self>) {
        let displaced = crate::ui::keymap::effective_bindings(cx)
            .into_iter()
            .chain(crate::ui::keymap::extra_bindings(cx))
            .find(|(a, k)| *k == spec && *a != action)
            .map(|(a, _)| a);
        // A trailing "…" on an action name marks a command that opens
        // something; it is not punctuation, and inside a sentence it reads as
        // the sentence trailing off — "Rename Tab… took the shortcut from".
        let in_prose = |name: &str| name.trim_end_matches('…').to_string();
        let note = displaced.as_ref().map(|other| {
            t_fmt(
                L10nKey::AppKeybindingDisplacedNote,
                &[
                    (
                        "action",
                        &in_prose(&crate::ui::keymap::action_entry(&action).1),
                    ),
                    (
                        "previous",
                        &in_prose(&crate::ui::keymap::action_entry(other).1),
                    ),
                ],
            )
        });
        self.update_config(cx, |cfg| {
            if let Some(other) = &displaced {
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

    pub(crate) fn restore_default_keybindings(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Nothing here is recoverable: the overrides are dropped from config
        // and the only record of them was the config.
        if cx.global::<Config>().keybindings.is_empty() {
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            t(crate::ui::i18n::L10nKey::SettingsRestoreAllDefaults),
            Some(t(crate::ui::i18n::L10nKey::SettingsRestoreAllDefaultsBody)),
            &crate::ui::confirm_answers(
                t(crate::ui::i18n::L10nKey::SettingsRestoreAllDefaults),
                t(crate::ui::i18n::L10nKey::Cancel),
            ),
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(0) = answer.await else { return };
            let _ = this.update(cx, |this, cx| {
                this.restore_default_keybindings_confirmed(cx)
            });
        })
        .detach();
    }

    fn restore_default_keybindings_confirmed(&mut self, cx: &mut Context<Self>) {
        self.update_config(cx, |cfg| cfg.keybindings.clear());
        crate::ui::keymap::rebind(cx);
        if let Some(s) = self.active_settings_mut() {
            s.recording = None;
            s.rebinding_note = None;
        }
        cx.notify();
    }

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

    pub(crate) fn set_keybinding_prefix(&mut self, prefix: &str, cx: &mut Context<Self>) {
        let prefix = prefix.to_string();
        self.update_config(cx, |cfg| cfg.prefix = prefix);
        crate::ui::keymap::rebind(cx);
        cx.notify();
    }
}

#[cfg(test)]
pub(crate) mod render_probe {
    use std::cell::Cell;

    thread_local! {
        static DRAWS: Cell<u64> = const { Cell::new(0) };
                                static BUDGET: Cell<Option<u64>> = const { Cell::new(None) };
    }

    pub(crate) fn record() {
        let n = DRAWS.get() + 1;
        DRAWS.set(n);
        if let Some(budget) = BUDGET.get()
            && n > budget
        {
            BUDGET.set(None);
            panic!(
                "the window drew more than {budget} frames without input: it never reached \
                 render idle (issue #243)"
            );
        }
    }

    pub(crate) fn arm(budget: u64) {
        DRAWS.set(0);
        BUDGET.set(Some(budget));
    }

    pub(crate) fn draws() -> u64 {
        DRAWS.get()
    }
}

impl Tty7App {
    fn render_remote_workspace_strip(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.tabs.is_empty() {
            return None;
        }
        let status = self.remote_status(cx)?;
        let machine = self.remote_machine_label(cx);
        let message = status.strip_message(&machine)?;
        // While an install runs, the strip is that install's progress: the
        // refusal it is answering is no longer the news, and the button that
        // started it would only start a second one.
        let installing = self.remote_strip_progress(cx);
        let action = installing
            .is_none()
            .then(|| self.remote_strip_action(&status, cx))
            .flatten();
        let theme = cx.theme();
        let message = match installing {
            Some(phase) => format!(
                "{machine} — {}",
                crate::ui::remote_workspace::install_phase_caption(phase)
            ),
            None => message,
        };
        let bar = gpui_component::v_flex()
            .occlude()
            .gap(px(6.))
            .px_3()
            .py_1p5()
            .rounded_lg()
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .shadow_md()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                gpui_component::h_flex()
                    .items_center()
                    .gap_2()
                    .child(gpui_component::Icon::new(gpui_component::IconName::Globe))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .child(message),
                    )
                    .when_some(action, |this, (label, action)| {
                        use gpui_component::Sizable as _;
                        use gpui_component::button::ButtonVariants as _;
                        this.child(
                            gpui_component::button::Button::new("remote-status-action")
                                .label(label)
                                .primary()
                                .small()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.run_strip_action(action.clone(), window, cx);
                                })),
                        )
                    }),
            )
            .when_some(installing, |this, phase| {
                this.child(crate::ui::remote_workspace::install_progress_bar(phase, cx))
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

#[derive(Clone, Debug, Default)]
pub(crate) struct ForwardRoute {
    pane_id: u64,
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

    /// The list a forward request answered with, or `None` when it did not
    /// answer at all.
    ///
    /// The two are not the same and the panel has to be able to tell them
    /// apart: an empty list is a pane with no forwards left, while a request
    /// that failed says nothing about what the far side still has. Reporting
    /// the second as the first is what blanked the panel whenever the daemon
    /// was briefly unreachable.
    fn forwards(
        reply: anyhow::Result<crate::daemon::protocol::DaemonMsg>,
    ) -> Option<Vec<crate::daemon::protocol::ManagedForward>> {
        match reply {
            Ok(crate::daemon::protocol::DaemonMsg::ForwardList(list)) => Some(list),
            Ok(other) => {
                log::warn!("unexpected reply to a workspace forward request: {other:?}");
                None
            }
            Err(e) => {
                log::warn!("a workspace forward request failed: {e}");
                None
            }
        }
    }

    pub(crate) fn list(&self) -> Vec<crate::daemon::protocol::ManagedForward> {
        let Some(req) = self.workspace_op(crate::daemon::protocol::WorkspaceOp::ListForwards)
        else {
            return crate::terminal::RemoteTerminal::list_forwards(self.pane_id);
        };
        Self::forwards(crate::terminal::RemoteTerminal::on_workspace(req)).unwrap_or_default()
    }

    pub(crate) fn add(
        &self,
        rule: crate::daemon::protocol::SshForwardRule,
    ) -> Option<Vec<crate::daemon::protocol::ManagedForward>> {
        let Some(req) = self
            .workspace_op(crate::daemon::protocol::WorkspaceOp::AddForward { rule: rule.clone() })
        else {
            return crate::terminal::RemoteTerminal::add_forward(self.pane_id, rule);
        };
        Self::forwards(crate::terminal::RemoteTerminal::on_workspace(req))
    }

    /// `None` when the teardown never happened — the route could not be
    /// addressed at all, or the daemon did not answer. `Some` means the far
    /// side acted, and the list is what it still holds.
    ///
    /// The distinction from `forwards` matters more here than on any read
    /// path. A workspace teardown is the last chance to release forwards that
    /// the daemon otherwise keeps bound for the rest of its life: nothing
    /// daemon-side reaps a workspace's forwards, unlike a pane's, which
    /// `DaemonPane::drop` always takes down. Answering `Vec::new()` for "never
    /// asked" made every one of those leaks read as a clean teardown.
    pub(crate) fn teardown(&self) -> Option<Vec<crate::daemon::protocol::ManagedForward>> {
        let req = self.workspace_op(crate::daemon::protocol::WorkspaceOp::TeardownForwards)?;
        Self::forwards(crate::terminal::RemoteTerminal::on_workspace(req))
    }

    pub(crate) fn remove(
        &self,
        forward_id: u64,
    ) -> Option<Vec<crate::daemon::protocol::ManagedForward>> {
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
        #[cfg(test)]
        render_probe::record();
        let prof = crate::ui::perf::enabled().then(std::time::Instant::now);
        // Every window's root is this view, so setting the rem here is what
        // makes `ui_font_size` reach the whole interface — the rem ladder
        // (`text_sm`, `text_xs`, `rems(..)`) resolves against it, and the
        // terminal grid, sized in absolute px from `font_size`, does not move.
        window.set_rem_size(px(cx.global::<Config>().ui_font_size));
        self.claim_pending_tab(window, cx);
        self.touch_active_tab();
        self.declare_displayed_panes(cx);
        self.scm_sync_watchers(window, cx);
        if cx.has_active_drag() {
            crate::ui::reorder::clear_pending(&self.reorder);
            crate::ui::pane_drag::clear_landing(&self.pane_drag);
        } else {
            // Taken first either way: this is what ends the drag, and the merge
            // below must not find the tab it just moved still in the air.
            let order = crate::ui::reorder::take_pending(&self.reorder);
            if let Some((tab, zone)) = self.tab_merge.take() {
                self.merge_tab(tab, zone, window, cx);
            } else if let Some(order) = order {
                self.apply_tab_order(&order, cx);
            }
            // Also what ends the pane drag, so it is taken whichever of the two
            // readings the last frame left behind.
            let landing = crate::ui::pane_drag::take_landing(&self.pane_drag);
            if let Some((pane, at)) = self.pane_detach.take() {
                self.detach_pane(pane, at, window, cx);
            } else if let Some((from, zone)) = landing {
                self.drop_pane(from, zone, window, cx);
            }
        }
        // Windows has no closed-hand cursor and gpui answers `ClosedHand` with
        // the plain arrow there, which would drop the grip's pointing hand the
        // instant the drag it advertised began.
        let held = if cfg!(target_os = "windows") {
            gpui::CursorStyle::PointingHand
        } else {
            gpui::CursorStyle::ClosedHand
        };
        if (self.reorder.borrow().is_some() || self.pane_drag.borrow().is_some())
            && cx.active_drag_cursor_style() != Some(held)
        {
            cx.set_active_drag_cursor_style(held, window);
        }
        let vertical = matches!(cx.global::<Config>().tab_bar_position, TabBarPosition::Left)
            && !self.tabs.is_empty();
        // Asked through the predicate the right panel sizes itself against —
        // two spellings of "is the rail up" is one more than the layout can
        // afford to have disagree.
        let rail = self.sidebar_open(cx);
        // Both read before the strip and the sidebar are built: a tab held out
        // over the layout suspends the reorder, which is what those two ask
        // what to draw, and a pane held over *them* is measured against where
        // they put their tabs last frame — which is what they are about to
        // blank and write again.
        let tab_landing = self.tab_landing(window, cx);
        let detach_caret = self.detach_caret(window, cx);
        let strip = self.tab_strip(!vertical, window, cx);
        let sidebar = rail.then(|| self.tab_sidebar(window, cx));
        let ssh_status = self
            .tabs
            .get(self.active)
            .and_then(|t| t.pane.focused_or_first(window, cx))
            .and_then(|leaf| self.render_ssh_status_strip(&leaf, cx));
        let body = match self.tabs.get(self.active) {
            None => self.render_home(cx).into_any_element(),
            Some(active_tab) => {
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
                        let several = active_tab.pane.leaves().len() > 1;
                        let chrome = crate::ui::pane::PaneChrome {
                            dim_inactive: several && cx.global::<Config>().dim_inactive_panes,
                            rearrangeable: several,
                            hovered: self.pane_hover.clone(),
                            lifted: crate::ui::pane_drag::lifted(&self.pane_drag),
                            drag: self.pane_drag.clone(),
                        };
                        active_tab.pane.render(&chrome, window, cx)
                    }
                }
            }
        };

        let title_bar = TitleBar::new()
            .h(px(TITLE_BAR_HEIGHT))
            .bg(cx.theme().transparent)
            .border_color(cx.theme().transparent)
            .child(strip);
        let body_area = div()
            .flex_1()
            .relative()
            .overflow_hidden()
            .child(
                gpui::canvas(
                    {
                        let area = self.pane_area.clone();
                        move |bounds, _window, _cx| area.set(Some(bounds))
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .child(body)
            .when_some(self.pane_landing(window, cx), |this, el| this.child(el))
            .when_some(tab_landing, |this, el| this.child(el))
            .when_some(ssh_status, |this, el| this.child(el))
            .when_some(self.render_remote_workspace_strip(cx), |this, el| {
                this.child(el)
            })
            .when_some(self.render_remote_input_notice(cx), |this, el| {
                this.child(el)
            });

        // One decision for the whole document surface. Docked, exactly one of
        // the two surfaces is drawn — a column has one child, and two `flex_1`
        // siblings would split it and fight — so `overlay_top` stops ordering a
        // pair and starts choosing between them. Filling, nothing changes: both
        // are rendered, ordered by `overlay_top`, and the front one wins on
        // paint order as it always has.
        let document_dock_px = self.document_dock_px(window, cx);
        // Where the docked header sits. Everywhere but macOS the title bar
        // spans the workspace and leaves the strip above the column empty, so
        // the header goes up into it; on macOS the column already reaches the
        // top of the window and its own first row lands there.
        let document_chrome = if cfg!(target_os = "macos") {
            crate::ui::document_column::DocumentChrome::Dock
        } else {
            crate::ui::document_column::DocumentChrome::DockHoisted
        };
        let document_header = document_dock_px
            .is_some()
            .then(|| self.render_document_header(document_chrome, window, cx))
            .flatten();
        let (overlays, document_column) = match document_dock_px {
            Some(w) => (
                Vec::new(),
                self.render_document_column(w, document_chrome, window, cx),
            ),
            None => {
                let diff_overlay = self.render_diff_overlay(
                    crate::ui::document_column::DocumentChrome::Fill,
                    window,
                    cx,
                );
                let code_overlay = self.render_code_overlay(
                    crate::ui::document_column::DocumentChrome::Fill,
                    window,
                    cx,
                );
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
                (
                    pair.into_iter()
                        .filter_map(|(_, el)| el)
                        .collect::<Vec<gpui::AnyElement>>(),
                    None,
                )
            }
        };
        let document_px = document_column
            .as_ref()
            .map_or(0., |_| document_dock_px.unwrap_or_default());

        let right_panel = self.render_right_panel(window, cx);
        // A docked document takes the same fork the right panel does: on
        // Windows and Linux the window controls live at the right end of the
        // title bar, so the bar has to span the workspace rather than sit
        // inside the terminal column with a column drawn to the right of it.
        let panel_below_title_bar =
            (right_panel.is_some() || document_column.is_some()) && !cfg!(target_os = "macos");
        let (column_title_bar, spanning_title_bar) = if panel_below_title_bar {
            (None, Some(title_bar))
        } else {
            (Some(title_bar), None)
        };
        let (column_overlays, hoisted_overlays) = if panel_below_title_bar {
            (Vec::new(), overlays)
        } else {
            (overlays, Vec::new())
        };
        let panel_px = if right_panel.is_some() {
            self.right_panel_px(window, cx)
        } else {
            0.
        };
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
            .when_some(document_column, |this, column| this.child(column))
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
                    .relative()
                    .child(
                        div()
                            .relative()
                            .flex_none()
                            // One patch per column below, rather than one for
                            // both: each carries the left border its own column
                            // carries, so the rule between the document and the
                            // detail panel runs the full height of the window
                            // instead of stopping at the title bar.
                            .when(panel_px > 0., |this| {
                                this.child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .bottom_0()
                                        .right_0()
                                        .w(px(panel_px))
                                        .bg(crate::ui::theme::workspace_surface_color(cx))
                                        .border_l_1()
                                        .border_color(cx.theme().sidebar_border),
                                )
                            })
                            .when(document_px > 0., |this| {
                                this.child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .bottom_0()
                                        .right(px(panel_px))
                                        .w(px(document_px))
                                        .bg(crate::ui::theme::workspace_surface_color(cx))
                                        .border_l_1()
                                        .border_color(cx.theme().sidebar_border),
                                )
                            })
                            .child(bar)
                            // The document's header, in the strip the spanning
                            // title bar leaves empty above its column. Drawn
                            // after the bar so it sits over the tab strip's
                            // slack — and stopping short of the trailing
                            // chrome, which is only in the way when the detail
                            // panel is closed and this column is the one at the
                            // window's right edge.
                            .when_some(document_header, |this, header| {
                                this.child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .h(px(TITLE_BAR_HEIGHT))
                                        .right(px(panel_px))
                                        .w(px(document_px))
                                        .when(panel_px <= 0., |d| {
                                            d.pr(px(crate::ui::tab_strip::trailing_chrome_w()))
                                        })
                                        .child(header),
                                )
                            }),
                    )
                    .child(panel_row)
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

        let window_bg = crate::ui::theme::workspace_background(cx);
        let bg_image = window_background_image_layer(cx);
        let settings_bg = crate::ui::theme::overlay_background(cx);

        let settings_overlay = self.settings.is_some().then(|| {
            div()
                .absolute()
                .inset_0()
                .occlude()
                // Opaque on purpose: the settings panel must never let the
                // workspace translucency (window opacity / backdrop material)
                // show through, even at window edges during a resize. The
                // preset's gradient fill is preserved, just with alpha 1;
                // `render_settings` repaints the theme image over it.
                .bg(settings_bg)
                .child(self.render_settings(window, cx))
        });

        let root =
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
                .on_action(cx.listener(|this, _: &ToggleSwitcher, window, cx| {
                    this.toggle_switcher(window, cx)
                }))
                .on_action(cx.listener(|this, _: &StopWorkspace, window, cx| {
                    let id = this.workspace;
                    this.stop_workspace(id, window, cx);
                }))
                .on_action(cx.listener(|this, _: &DeleteWorkspace, window, cx| {
                    let id = this.workspace;
                    this.delete_workspace(id, window, cx);
                }))
                .on_action(cx.listener(|this, _: &NewWorkspace, window, cx| {
                    this.open_workspace_form(window, cx);
                }))
                .on_action(cx.listener(|this, _: &CloseActiveTab, window, cx| {
                    if !this.editor_close_active_if_focused(window, cx) {
                        this.close_pane(window, cx)
                    }
                }))
                .on_action(cx.listener(|this, _: &SplitRight, window, cx| {
                    this.split(Axis::Horizontal, window, cx)
                }))
                .on_action(cx.listener(|this, _: &SplitDown, window, cx| {
                    this.split(Axis::Vertical, window, cx)
                }))
                .on_action(cx.listener(|this, _: &FocusNextPane, window, cx| {
                    this.cycle_pane(true, window, cx)
                }))
                .on_action(cx.listener(|this, _: &FocusPrevPane, window, cx| {
                    this.cycle_pane(false, window, cx)
                }))
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
                .on_action(cx.listener(|this, _: &SwapPaneNext, window, cx| {
                    this.swap_pane(true, window, cx)
                }))
                .on_action(cx.listener(|this, _: &SwapPanePrev, window, cx| {
                    this.swap_pane(false, window, cx)
                }))
                .on_action(
                    cx.listener(|this, _: &NextTab, window, cx| this.tab_switch(true, window, cx)),
                )
                .on_action(
                    cx.listener(|this, _: &PrevTab, window, cx| this.tab_switch(false, window, cx)),
                )
                .on_action(cx.listener(|this, _: &ActivateTab1, window, cx| {
                    this.activate_visual(0, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ActivateTab2, window, cx| {
                    this.activate_visual(1, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ActivateTab3, window, cx| {
                    this.activate_visual(2, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ActivateTab4, window, cx| {
                    this.activate_visual(3, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ActivateTab5, window, cx| {
                    this.activate_visual(4, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ActivateTab6, window, cx| {
                    this.activate_visual(5, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ActivateTab7, window, cx| {
                    this.activate_visual(6, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ActivateTab8, window, cx| {
                    this.activate_visual(7, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ActivateTab9, window, cx| {
                    this.activate_visual(8, window, cx)
                }))
                .on_action(cx.listener(|this, _: &IncreaseFontSize, _window, cx| {
                    this.change_font_size(FONT_SIZE_STEP, cx)
                }))
                .on_action(cx.listener(|this, _: &DecreaseFontSize, _window, cx| {
                    this.change_font_size(-FONT_SIZE_STEP, cx)
                }))
                .on_action(
                    cx.listener(|this, _: &ResetFontSize, _window, cx| this.reset_font_size(cx)),
                )
                .on_action(cx.listener(|this, _: &TogglePalette, window, cx| {
                    this.toggle_palette(window, cx)
                }))
                .on_action(cx.listener(|this, _: &ReopenClosedTab, window, cx| {
                    this.reopen_closed_tab(window, cx)
                }))
                .on_action(cx.listener(|this, _: &ToggleMaximizePane, window, cx| {
                    this.toggle_maximize(window, cx)
                }))
                .on_action(
                    cx.listener(|_, _: &ToggleFullscreen, window, _cx| window.toggle_fullscreen()),
                )
                .on_action(cx.listener(|this, _: &ToggleTabSidebar, _window, cx| {
                    this.toggle_tab_sidebar(cx)
                }))
                .on_action(
                    cx.listener(|this, _: &ToggleLeftPanel, _window, cx| {
                        this.toggle_left_panel(cx)
                    }),
                )
                .on_action(cx.listener(|this, _: &ToggleRightPanel, _window, cx| {
                    this.toggle_right_panel(cx)
                }))
                .on_action(cx.listener(|this, _: &ShowRightPanelInfo, _window, cx| {
                    this.set_right_panel_tab(crate::core::config::RightPanelTab::Info, cx)
                }))
                .on_action(cx.listener(|this, _: &ShowRightPanelChanges, _window, cx| {
                    this.set_right_panel_tab(crate::core::config::RightPanelTab::Scm, cx)
                }))
                .on_action(cx.listener(|this, _: &ShowRightPanelFiles, _window, cx| {
                    this.set_right_panel_tab(crate::core::config::RightPanelTab::Files, cx)
                }))
                .on_action(
                    cx.listener(|this, _: &ScmToggleGraph, _window, cx| this.scm_toggle_graph(cx)),
                )
                .on_action(cx.listener(|this, _: &ToggleDiffViewMode, _window, cx| {
                    this.toggle_diff_view_mode(cx)
                }))
                .on_action(cx.listener(|this, _: &ToggleDocumentFill, _window, cx| {
                    this.toggle_document_fill(cx)
                }))
                .on_action(cx.listener(|this, _: &DocumentWidthThird, _window, cx| {
                    this.set_document_ratio(crate::core::config::DOCUMENT_RATIO_THIRD, cx)
                }))
                .on_action(cx.listener(|this, _: &DocumentWidthHalf, _window, cx| {
                    this.set_document_ratio(crate::core::config::DOCUMENT_RATIO_HALF, cx)
                }))
                .on_action(
                    cx.listener(|this, _: &DocumentWidthTwoThirds, _window, cx| {
                        this.set_document_ratio(crate::core::config::DOCUMENT_RATIO_TWO_THIRDS, cx)
                    }),
                )
                .on_action(cx.listener(|this, _: &ScmCommit, window, cx| {
                    this.run_scm_action(ScmIntent::Commit, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ScmCommitAmend, window, cx| {
                    this.run_scm_action(ScmIntent::CommitAmend, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ScmStageAll, window, cx| {
                    this.run_scm_action(ScmIntent::StageAll, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ScmUnstageAll, window, cx| {
                    this.run_scm_action(ScmIntent::UnstageAll, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ScmDiscardAll, window, cx| {
                    this.run_scm_action(ScmIntent::DiscardAll, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ScmRefresh, window, cx| {
                    this.run_scm_action(ScmIntent::Refresh, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ScmSync, window, cx| {
                    this.run_scm_action(ScmIntent::Sync, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ScmPush, window, cx| {
                    this.run_scm_action(ScmIntent::Push, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ScmPull, window, cx| {
                    this.run_scm_action(ScmIntent::Pull, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ScmFetch, window, cx| {
                    this.run_scm_action(ScmIntent::Fetch, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ScmCheckoutBranch, window, cx| {
                    this.run_scm_action(ScmIntent::CheckoutBranch, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ScmCreateBranch, window, cx| {
                    this.run_scm_action(ScmIntent::CreateBranch, window, cx)
                }))
                .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                    this.toggle_settings(window, cx)
                }))
                .on_action(cx.listener(|this, _: &RestartDaemon, window, cx| {
                    this.restart_window_daemon(window, cx)
                }))
                .on_action(
                    cx.listener(|this, _: &ToggleSftp, window, cx| this.toggle_sftp(window, cx)),
                )
                .on_action(cx.listener(|this, _: &ShowSshForwards, window, cx| {
                    this.show_ssh_forwards(window, cx)
                }))
                .on_action(cx.listener(|this, _: &ToggleCodePanel, window, cx| {
                    this.toggle_code_panel(window, cx)
                }))
                .on_action(cx.listener(|this, _: &EditorSave, window, cx| {
                    if !this.editor_has_focus(window, cx) {
                        cx.propagate();
                        return;
                    }
                    this.editor_save_active(window, cx)
                }))
                .on_action(
                    cx.listener(|this, _: &Quit, window, cx| this.quit_stop_sessions(window, cx)),
                )
                .on_action(cx.listener(|this, _: &OpenSshProfiles, window, cx| {
                    this.open_settings_section(SettingsSection::Ssh, window, cx)
                }))
                .on_action(cx.listener(|this, _: &RestartSshSession, window, cx| {
                    this.restart_ssh_session(window, cx)
                }))
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
                .on_action(cx.listener(|this, _: &ShowKeyboardShortcuts, window, cx| {
                    this.open_settings_section(SettingsSection::Keybindings, window, cx)
                }))
                .on_action(cx.listener(|this, _: &About, window, cx| {
                    this.open_settings_section(SettingsSection::About, window, cx)
                }))
                .on_action(cx.listener(|this, _: &CheckForUpdates, window, cx| {
                    this.check_for_updates_now(window, cx)
                }))
                .on_action(cx.listener(|_, _: &HideApp, _window, cx| cx.hide()))
                .on_action(cx.listener(|_, _: &HideOthers, _window, cx| cx.hide_other_apps()))
                .on_action(cx.listener(|_, _: &ShowAll, _window, cx| cx.unhide_other_apps()))
                .on_action(
                    cx.listener(|_, _: &MinimizeWindow, window, _cx| window.minimize_window()),
                )
                .on_action(cx.listener(|_, _: &ZoomWindow, window, _cx| window.zoom_window()))
                .on_action(
                    cx.listener(|_, _: &OpenDocumentation, _window, cx| cx.open_url(DOCS_URL)),
                )
                .on_action(cx.listener(|_, _: &OpenDiscord, _window, cx| cx.open_url(DISCORD_URL)))
                .on_action(cx.listener(|_, _: &ReportIssue, _window, cx| cx.open_url(ISSUES_URL)))
                .children(bg_image)
                .child(main_layout)
                // Window-level because the strip lives in the title bar and the
                // sidebar down the side: the caret between two tabs is in
                // neither of the boxes the rest of the drag feedback is drawn
                // in.
                .when_some(detach_caret, |this, caret| this.child(caret))
                .when_some(settings_overlay, |this, overlay| this.child(overlay))
                // Window-level, like the switcher and the palette: the prompt
                // blocks the whole app, so its scrim has to reach the title bar
                // and the side panels too. Parented to `body_area` it was
                // clipped to the terminal area, which read as "only the
                // terminal is busy".
                .when_some(self.render_worktree_prompt_overlay(cx), |this, el| {
                    this.child(el)
                })
                // Same reason, and the ssh prompt has more claim to it than any
                // of them: nothing in the window can proceed until the password
                // is answered, so the scrim has to cover the whole window and
                // not stop at the terminal area.
                .when_some(self.render_ssh_prompt_overlay(window, cx), |this, el| {
                    this.child(el)
                })
                .children(self.render_switcher(window, cx))
                .when_some(self.palette.clone(), |this, palette| this.child(palette))
                .children(gpui_component::Root::render_notification_layer(window, cx));

        if let Some(start) = prof {
            crate::ui::perf::record("window", start.elapsed());
        }
        root
    }
}

/// Orders tab indices most-recently-used first from their `last_used` stamps.
/// A zero stamp means the tab was never activated, and those keep strip order
/// at the back. `active` leads regardless — its own stamp only lands on the
/// next frame.
fn mru_order(stamps: &[u64], active: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..stamps.len()).collect();
    order.sort_by_key(|&i| (stamps[i] == 0, std::cmp::Reverse(stamps[i]), i));
    if let Some(pos) = order.iter().position(|&i| i == active) {
        let lead = order.remove(pos);
        order.insert(0, lead);
    }
    order
}

fn tab_to_session(tab: &Tab, cx: &App) -> SessionTab {
    SessionTab {
        name: tab.name.clone(),
        pane: pane_to_session(&tab.pane, cx),
        sidebar_group: tab.sidebar_group.borrow().clone(),
        tree_id: None,
    }
}

fn agent_resume_command(
    agent: &Option<crate::core::cli_agent::CLIAgent>,
    session_id: Option<&str>,
    launch_argv: Option<&[String]>,
    cx: &App,
) -> Option<String> {
    if !cx.global::<Config>().restore_agent_sessions {
        return None;
    }
    let agent = agent.as_ref()?;
    let Some(session_id) = session_id else {
        log::info!(
            "{}'s pane had no captured session id; it comes back as a plain shell",
            agent.display_name()
        );
        return None;
    };
    agent.resume_command(session_id, launch_argv)
}

fn pane_to_session(pane: &Pane, cx: &App) -> SessionPane {
    match pane {
        Pane::Leaf(PaneSlot::Connecting(pending)) => {
            let spawn = &pending.read(cx).spawn;
            SessionPane::Leaf {
                cwd: spawn.working_directory.clone(),
                pane_id: spawn.restore_pane,
                shell: spawn.shell.clone(),
                ssh_spec: None,
                agent: spawn.agent,
                agent_session_id: spawn.agent_session_id.clone(),
                agent_launch_argv: spawn.agent_launch_argv.clone(),
            }
        }
        Pane::Leaf(PaneSlot::Ready(view)) => {
            let view = view.read(cx);
            SessionPane::Leaf {
                cwd: view.spawnable_cwd(),
                pane_id: Some(view.pane_id),
                // `None` for a pane this window attached to rather than
                // spawned: it never knew what was on the other end. The tree
                // does — the daemon records it — and that is what a restore
                // reads, so the gap here costs nothing it can see.
                shell: view.shell_spec(),
                ssh_spec: view.ssh_spec(),
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
        Pane::Empty => SessionPane::Leaf {
            cwd: None,
            pane_id: None,
            shell: None,
            ssh_spec: None,
            agent: None,
            agent_session_id: None,
            agent_launch_argv: None,
        },
    }
}

/// The daemon's account of which panes are alive, or `None` when it could not
/// be asked at all.
///
/// The distinction is the point: a pane absent from a *successful* listing is
/// genuinely gone and may be respawned, while a failed `List` says nothing
/// about any pane. Flattening the failure into an empty map made one transient
/// RPC error read as "every pane is dead", and the restore then spawned fresh
/// shells over all of them — the same destruction-by-inference this file's
/// restore path is built to avoid.
pub(crate) fn alive_panes_on(
    route: &crate::terminal::PaneRoute,
) -> Option<std::collections::HashMap<u64, Option<String>>> {
    if !matches!(route, crate::terminal::PaneRoute::Local) {
        return Some(std::collections::HashMap::new());
    }
    match crate::terminal::RemoteTerminal::try_list_panes_on(route) {
        Ok(list) => Some(
            list.into_iter()
                .filter(|p| p.alive)
                .map(|p| (p.pane_id, p.owner))
                .collect(),
        ),
        Err(e) => {
            log::warn!("could not list panes ({e}); leaving each attach to decide");
            None
        }
    }
}

/// Whether this window may stand on `id` — as the pane it attaches to, or as
/// the dead predecessor whose screen a fresh pane opens showing.
///
/// Liveness deliberately does not come into it, and that is the whole point.
/// A pane missing from the listing is usually one whose daemon has just
/// restarted, which is exactly when its stored screen is worth asking for.
/// Ruling the id out there threw away the only thing that could ask: the
/// window spawned a pane that had never heard of a predecessor, so no attach
/// was tried, no restore was requested, and the screen the daemon still had on
/// disk was swept a tick later, unread.
///
/// There used to be a second predicate here that also required the id to be
/// listed, and the attach site consulted it. Nothing does now: the attach is
/// simply tried, and a pane that really is gone fails it and falls through to
/// the fresh spawn — the same outcome the listing was consulted to predict,
/// reached by asking the daemon instead of guessing ahead of it.
///
/// Ownership does come into it. Another workspace's pane is not this window's
/// to attach to, and its screen is not this window's to show.
fn pane_free_for(
    alive: Option<&std::collections::HashMap<u64, Option<String>>>,
    id: u64,
    owner: crate::core::session::WorkspaceId,
) -> bool {
    let Some(alive) = alive else {
        // No listing to consult. Attaching is the safe guess in both
        // directions: if the daemon is really unreachable the attach fails and
        // the pane falls to the fresh-spawn path anyway, while spawning fresh
        // on a hunch destroys a session that was merely hard to reach.
        return true;
    };
    match alive.get(&id) {
        None => true,
        Some(None) => true,
        Some(Some(recorded)) => {
            // Only a workspace id is a claim. Anything else is a client
            // stamping its own name — `tty7` before this release wrote a
            // literal "tty7-cli" — and refusing on it strands every pane the
            // CLI ever made, respawning over a live shell the tree just told
            // us belongs here.
            let Ok(recorded) = recorded.parse::<crate::core::session::WorkspaceId>() else {
                return true;
            };
            let ours = recorded == owner;
            if !ours {
                log::warn!(
                    "restore: pane {id} is owned by workspace {recorded}, not {owner}; \
                     spawning fresh instead of attaching to it"
                );
            }
            ours
        }
    }
}

fn tabs_from_session(
    workspace: Option<&crate::terminal::PaneWorkspace>,
    owner: WorkspaceId,
    session: Option<Session>,
    font_size: f32,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> (Vec<Tab>, usize, usize) {
    let Some(session) = session.filter(|s| !s.tabs.is_empty()) else {
        return (Vec::new(), 0, 0);
    };
    let alive = alive_panes_on(&crate::terminal::PaneRoute::for_workspace(workspace));
    let mut tabs: Vec<Tab> = Vec::with_capacity(session.tabs.len());
    let mut dropped = 0usize;
    for st in &session.tabs {
        let Some(pane) = session_to_pane(
            workspace,
            owner,
            &st.pane,
            alive.as_ref(),
            font_size,
            window,
            cx,
        ) else {
            // The layout coming back is the product's headline claim, so a tab
            // that quietly does not is worth a sentence rather than a log line.
            log::error!("dropping a restored tab: no pane in it could be started");
            dropped += 1;
            continue;
        };
        tabs.push(Tab {
            pane,
            name: st.name.clone(),
            last_focused: None,
            zoomed: None,
            diff_overlay: None,
            code: None,
            overlay_top: OverlayTop::default(),
            document_layout: None,
            sidebar_group: std::cell::RefCell::new(st.sidebar_group.clone()),
            tree_id: std::cell::Cell::new(
                st.tree_id
                    .unwrap_or_else(tty7_core::core::machine::TabId::new),
            ),
            last_used: std::cell::Cell::new(0),
        });
    }
    let active = session.active.min(tabs.len().saturating_sub(1));
    (tabs, active, dropped)
}

fn leaf_shares_the_window_daemon(window_is_remote: bool, leaf_is_native_ssh: bool) -> bool {
    !(window_is_remote && leaf_is_native_ssh)
}

fn session_to_pane(
    workspace: Option<&crate::terminal::PaneWorkspace>,
    owner: WorkspaceId,
    sp: &SessionPane,
    alive: Option<&std::collections::HashMap<u64, Option<String>>>,
    font_size: f32,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> Option<Pane> {
    match sp {
        SessionPane::Leaf {
            cwd,
            pane_id,
            shell,
            ssh_spec,
            agent,
            agent_session_id,
            agent_launch_argv,
        } => {
            let same_daemon =
                leaf_shares_the_window_daemon(workspace.is_some(), ssh_spec.is_some());
            let restore = match workspace.is_some() {
                true => (*pane_id).filter(|_| same_daemon),
                // Deliberately not an aliveness test: a dead pane's id is
                // what the restore is keyed on, so it has to survive being
                // dead. The attach is still attempted first and still gives
                // way to a fresh spawn.
                false => (*pane_id).filter(|id| same_daemon && pane_free_for(alive, *id, owner)),
            };
            if restore.is_none()
                && let Some(spec) = ssh_spec.clone()
            {
                let resolved = crate::ui::ssh_connect::resolve_persisted_ssh_spec(spec, cx);
                match new_terminal_native(font_size, cwd.clone(), resolved, window, cx) {
                    Ok(view) => return Some(Pane::leaf(PaneSlot::Ready(view))),
                    Err(e) => log::error!("restoring native SSH pane failed: {e}"),
                }
            }
            let view = match new_terminal(
                workspace.cloned(),
                Some(owner),
                font_size,
                cwd.clone(),
                restore,
                shell.clone(),
                window,
                cx,
            ) {
                Ok(view) => view,
                Err(e) => {
                    log::error!("restoring pane failed: {e}");
                    return None;
                }
            };
            match &view {
                PaneSlot::Ready(terminal) if !terminal.read(cx).restored() => {
                    if let Some(cmd) = agent_resume_command(
                        agent,
                        agent_session_id.as_deref(),
                        agent_launch_argv.as_deref(),
                        cx,
                    ) {
                        terminal.read(cx).run_command_line(&cmd);
                    }
                }
                PaneSlot::Ready(_) => {}
                PaneSlot::Connecting(pending) => {
                    pending.update(cx, |pending, _| {
                        pending.spawn.agent = *agent;
                        pending.spawn.agent_session_id = agent_session_id.clone();
                        pending.spawn.agent_launch_argv = agent_launch_argv.clone();
                    });
                }
            }
            Some(Pane::leaf(view))
        }
        SessionPane::Split { axis, ratio, a, b } => {
            let axis = match axis {
                SessionAxis::Horizontal => Axis::Horizontal,
                SessionAxis::Vertical => Axis::Vertical,
            };
            match (
                session_to_pane(workspace, owner, a, alive, font_size, window, cx),
                session_to_pane(workspace, owner, b, alive, font_size, window, cx),
            ) {
                (Some(a), Some(b)) => Some(Pane::split_node(axis, *ratio, a, b)),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
    }
}

pub(crate) fn new_terminal(
    workspace: Option<crate::terminal::PaneWorkspace>,
    owner: Option<WorkspaceId>,
    font_size: f32,
    working_directory: Option<std::path::PathBuf>,
    restore_pane: Option<u64>,
    shell: Option<ShellSpec>,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> anyhow::Result<PaneSlot> {
    if matches!(
        crate::terminal::PaneRoute::for_workspace(workspace.as_ref()),
        crate::terminal::PaneRoute::Local
    ) {
        let parts = TerminalView::spawn_shell_terminal_in(
            workspace,
            working_directory,
            restore_pane,
            shell,
            owner,
        )?;
        return Ok(PaneSlot::Ready(build_terminal_view(
            parts, font_size, window, cx,
        )));
    }

    let spawn = crate::ui::pending_pane::PendingSpawn {
        workspace,
        working_directory,
        restore_pane,
        shell,
        agent: None,
        agent_session_id: None,
        agent_launch_argv: None,
        owner,
        font_size,
    };
    // The same leak the reconnect banner had: this name is read out as
    // "Connecting to {machine}…" and "Could not reach {machine}", and a
    // `Profile` target spells itself as its config UUID (#485). The pane's own
    // notifications already resolve it through the live config.
    let machine = spawn
        .workspace
        .as_ref()
        .map(|w| crate::ui::remote_connect::target_label(cx, &w.target))
        .unwrap_or_else(|| t(L10nKey::AppLocalServerName).to_string());
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
                TerminalView::spawn_shell_terminal_in(
                    spawn.workspace.clone(),
                    spawn.working_directory.clone(),
                    spawn.restore_pane,
                    spawn.shell.clone(),
                    spawn.owner,
                )
                .map_err(|e| format!("{e:#}"))
            })
            .await;
        let _ = this.update_in(cx, |app, window, cx| {
            app.land_pane(slot_id, &pending, parts, font_size, window, cx);
        });
    })
    .detach();
}

fn build_terminal_view(
    parts: crate::terminal::view::ShellParts,
    font_size: f32,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) -> Entity<TerminalView> {
    // Where both spawn routes meet, and the last point at which a pane this
    // window made is certain to be known to it. See `tree_sync::note_spawned`.
    crate::ui::tree_sync::note_spawned(cx, parts.owner, parts.pane_id);
    let view = cx.new(|cx| {
        let mut view = TerminalView::from_shell_parts(parts, window, cx);
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
        |app, _view, _: &crate::terminal::view::AgentSessionChanged, _window, cx| {
            app.save_session(cx);
        },
    )
    .detach();
    cx.subscribe_in(
        &view,
        window,
        |app, view, _: &crate::terminal::view::AuthPromptReady, window, cx| {
            app.on_auth_prompt_ready(view.clone(), window, cx);
        },
    )
    .detach();
    watch_open_file_requests(&view, window, cx);
    watch_pane_focus(&view, window, cx);
    view
}

fn kill_pane_off_thread(route: crate::terminal::PaneRoute, pane_id: u64, cx: &mut App) {
    cx.background_executor()
        .spawn(async move { crate::terminal::RemoteTerminal::kill_pane_on(&route, pane_id) })
        .detach();
}

/// Routes the file links clicked in a pane to whatever the app opens files
/// with. Every pane needs this, however it was spawned — a link in an SSH pane
/// is the same click as a link in a local one.
fn watch_open_file_requests(
    view: &Entity<TerminalView>,
    window: &mut Window,
    cx: &mut Context<Tty7App>,
) {
    cx.subscribe_in(
        view,
        window,
        |app, _view, ev: &crate::terminal::view::OpenFileRequested, window, cx| {
            app.open_linked_file(&ev.path, ev.line, ev.column, ev.is_dir, window, cx);
        },
    )
    .detach();
}

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
    watch_open_file_requests(&view, window, cx);
    watch_pane_focus(&view, window, cx);
    Ok(view)
}

/// Reads the Settings "Shell Arguments" field as a command line (#551).
///
/// The field is one line of text standing in for the `shell.args` array, so
/// this and [`join_shell_args`] have to be exact inverses of each other — the
/// field is refilled from the array every time Settings opens, and committed
/// back on every blur. Quoting rules are the smallest set that can spell any
/// argv: single quotes take everything literally, double quotes take everything
/// literally except `\"` and `\\`, and whitespace outside quotes separates
/// words.
///
/// A lone backslash outside quotes is *not* an escape, unlike a POSIX shell:
/// these arguments are handed to the process as argv without a shell in
/// between, and `--dir C:\Users\me` has to keep its backslashes on the platform
/// where that is how a path is spelled. `#` starts no comment here either, for
/// the same reason: nothing in an argv list is a comment, and swallowing the
/// rest of the line would be the silent rewrite this whole change exists to
/// stop.
///
/// Returns `Err` when a quote never closes: that text names no argv at all.
pub(crate) fn split_shell_args(input: &str) -> Result<Vec<String>, ()> {
    let mut words = Vec::new();
    let mut current = String::new();
    // A word can be empty and still be a word — `''` is a real argument that
    // whitespace splitting cannot spell, so emptiness alone cannot end one.
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            // Only inside double quotes, and only for the two characters that
            // would otherwise be unspellable there.
            (Some('"'), '\\') => match chars.clone().next() {
                Some(next @ ('"' | '\\')) => {
                    chars.next();
                    current.push(next);
                }
                _ => current.push('\\'),
            },
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => {
                quote = Some(ch);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err(());
    }
    if started {
        words.push(current);
    }
    Ok(words)
}

/// Writes an argv array back into the Settings "Shell Arguments" field (#551).
///
/// The exact inverse of [`split_shell_args`], and quotes only what it must:
/// `join(" ")` cannot spell an argument that contains a space, so a legal
/// `"args": ["-c", "echo hi"]` used to refill as three words and re-commit as
/// three argv on the next blur. Arguments that need no quoting are written
/// bare, so the common `--login --color=auto` still reads as the user typed it.
pub(crate) fn join_shell_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| quote_shell_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_shell_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| c.is_whitespace() || c == '\'' || c == '"')
    {
        return arg.to_string();
    }
    // Single quotes need no escapes at all, so they are the first choice; an
    // argument containing one falls back to double quotes, which can escape.
    if !arg.contains('\'') {
        return format!("'{arg}'");
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    for c in arg.chars() {
        if c == '"' || c == '\\' {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// The one rule the "Start in" custom path lives by (#601): empty means unset
/// and saves; anything else must name a directory that exists, because the
/// daemon's picker skips a path that is not one and every new pane then
/// silently starts somewhere else. Settings refuses to save such a value and
/// marks it red — both decide through this, so the red line and the not-saved
/// config always agree. Local on purpose: this is the local daemon's config.
pub(crate) fn wd_path_saveable(path: &str) -> bool {
    let path = path.trim();
    path.is_empty() || std::path::Path::new(path).is_dir()
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

pub(crate) struct ParsedSshConnect {
    pub profile: crate::core::ssh_profile::SshProfile,
    pub proxy_jump: Option<String>,
    /// Whether the typed text named a port at all, either as `-p`/`-o Port=`
    /// or as a `:port` on the target.
    ///
    /// `profile.port` cannot answer this: it holds 22 both for text that asked
    /// for 22 and for text that asked for nothing, and the two have to merge
    /// differently against an alias from `~/.ssh/config` that sets its own
    /// port. `ssh -p 22 myalias` reaches port 22 even when the alias says
    /// 2222, because the command line outranks the config file.
    pub port_given: bool,
}

pub(crate) fn parse_ssh_connect_input(input: &str) -> Result<ParsedSshConnect, String> {
    use crate::core::ssh_profile::{SshProfile, parse_quick_connect};

    let mut words = parse_ssh_option_words(input)
        .map_err(|_| t(L10nKey::AppSshParseUnbalancedQuotes).to_string())?;
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
            return Err(t(L10nKey::AppSshParseNoRemoteCommands).to_string());
        }
        if let Some((flag, attached)) = ssh_short_flag(&word) {
            let value = if ssh_option_takes_value(flag) {
                if !attached.is_empty() {
                    attached
                } else {
                    i += 1;
                    match words.get(i) {
                        Some(v) => v.clone(),
                        None => {
                            return Err(t_fmt(
                                L10nKey::AppSshParseFlagNeedsValue,
                                &[("flag", &flag.to_string())],
                            ));
                        }
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
                            .ok_or_else(|| {
                                t_fmt(L10nKey::AppSshParseInvalidPort, &[("value", &value)])
                            })?,
                    )
                }
                'l' => user = Some(value),
                'i' => identities.push(value),
                'J' => jump = Some(value),
                'o' => apply_ssh_o_option(&value, &mut user, &mut port, &mut jump)?,
                _ => {}
            }
        } else if word.starts_with('-') {
            return Err(t_fmt(
                L10nKey::AppSshParseUnsupportedOption,
                &[("option", &word)],
            ));
        } else if target.is_none() {
            target = Some(word);
        } else {
            return Err("Remote commands aren't supported here".to_string());
        }
        i += 1;
    }

    let target = target.ok_or_else(|| t(L10nKey::AppSshParseEnterHost).to_string())?;
    let qc = parse_quick_connect(&target)
        .ok_or_else(|| t_fmt(L10nKey::AppSshParseBadHost, &[("host", &target)]))?;

    let mut profile = SshProfile::new(qc.host.clone());
    profile.host = qc.host;
    let port = port.or(qc.port);
    profile.port = port.unwrap_or(22);
    if let Some(user) = user.or(qc.user) {
        profile.user = user;
    }
    profile.identity_files = identities;

    Ok(ParsedSshConnect {
        profile,
        proxy_jump: jump,
        port_given: port.is_some(),
    })
}

/// What the typed text and a matching `~/.ssh/config` alias add up to.
///
/// The alias supplies the ground, and anything the text actually named wins
/// over it — the order `ssh` itself uses, where the command line outranks the
/// config file. "Actually named" is the whole difficulty: an empty user and an
/// empty identity list say nothing, but a port of 22 is indistinguishable from
/// silence unless the parser is asked, which is what `port_given` is for.
pub(crate) fn merge_typed_ssh_over_alias(
    parsed: ParsedSshConnect,
    resolved: Option<crate::core::ssh_config::ResolvedAlias>,
) -> (crate::core::ssh_profile::SshProfile, Option<String>) {
    let Some(resolved) = resolved else {
        return (parsed.profile, parsed.proxy_jump);
    };
    let mut p = resolved.profile;
    if !parsed.profile.user.is_empty() {
        p.user = parsed.profile.user;
    }
    if parsed.port_given {
        p.port = parsed.profile.port;
    }
    if !parsed.profile.identity_files.is_empty() {
        p.identity_files = parsed.profile.identity_files;
    }
    (p, parsed.proxy_jump.or(resolved.proxy_jump))
}

fn ssh_short_flag(word: &str) -> Option<(char, String)> {
    let rest = word.strip_prefix('-')?;
    if rest.is_empty() || rest.starts_with('-') {
        return None;
    }
    let mut chars = rest.chars();
    let flag = chars.next()?;
    Some((flag, chars.as_str().to_string()))
}

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
                    .ok_or_else(|| t_fmt(L10nKey::AppSshParseInvalidPort, &[("value", val)]))?,
            )
        }
        "proxyjump" => *jump = Some(val.to_string()),
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod window_drag_tests {
    use gpui::{
        Context, InteractiveElement as _, IntoElement, Modifiers, MouseButton, MouseDownEvent,
        MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, PlatformInput, Point, Render,
        Styled as _, TestAppContext, VisualTestContext, Window, div, point, px,
    };
    use std::cell::Cell;
    use std::rc::Rc;

    struct Host;
    impl Render for Host {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(super::title_bar_drag(
                div().id("stand-in-title-bar").w_full().h(px(40.)),
                "stand-in-title-bar",
                window,
                cx,
            ))
        }
    }

    struct HandleOverRow {
        occluded: bool,
    }
    impl Render for HandleOverRow {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let handle = div()
                .absolute()
                .top_0()
                .left(px(296.))
                .w(px(8.))
                .h_full()
                .cursor_col_resize()
                .on_mouse_down(MouseButton::Left, |_, window, _| window.refresh());
            let handle = if self.occluded {
                handle.occlude()
            } else {
                handle
            };
            div()
                .relative()
                .size_full()
                .child(super::title_bar_drag(
                    div().id("stand-in-title-bar").w_full().h(px(40.)),
                    "stand-in-title-bar",
                    window,
                    cx,
                ))
                .child(handle)
        }
    }

    struct PerFrameCellHost;
    impl Render for PerFrameCellHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let should_move = Rc::new(Cell::new(false));
            div().size_full().child(
                div()
                    .id("per-frame-cell")
                    .w_full()
                    .h(px(40.))
                    .on_mouse_down(MouseButton::Left, {
                        let should_move = should_move.clone();
                        move |_, _, _| should_move.set(true)
                    })
                    .on_mouse_move(move |_, window, _| {
                        if should_move.replace(false) {
                            window.start_window_move();
                        }
                    }),
            )
        }
    }

    fn down(at: Point<Pixels>) -> PlatformInput {
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        })
    }

    fn up(at: Point<Pixels>) -> PlatformInput {
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers::none(),
            click_count: 1,
        })
    }

    fn moved(at: Point<Pixels>, held: bool) -> PlatformInput {
        PlatformInput::MouseMove(MouseMoveEvent {
            position: at,
            pressed_button: held.then_some(MouseButton::Left),
            modifiers: Modifiers::none(),
        })
    }

    const ON_ROW: Point<Pixels> = Point {
        x: px(300.),
        y: px(20.),
    };

    fn drifted(at: Point<Pixels>) -> Point<Pixels> {
        point(at.x + px(12.), at.y + px(3.))
    }

    fn press_repaint_move(vcx: &mut VisualTestContext, at: Point<Pixels>) {
        vcx.update(|window, cx| {
            window.dispatch_event(moved(at, false), cx);
            window.dispatch_event(down(at), cx);
        });
        vcx.update(|window, _| window.refresh());
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            window.dispatch_event(moved(drifted(at), true), cx);
        });
        vcx.run_until_parked();
    }

    #[gpui::test]
    #[should_panic(expected = "not implemented")]
    fn the_arm_survives_a_repaint_between_press_and_move(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Host);
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        press_repaint_move(&mut vcx, ON_ROW);
    }

    #[gpui::test]
    fn a_per_frame_cell_loses_the_arm_to_the_same_repaint(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| PerFrameCellHost);
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        press_repaint_move(&mut vcx, ON_ROW);
    }

    #[gpui::test]
    fn a_press_alone_does_not_move_the_window(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Host);
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| {
            window.dispatch_event(moved(ON_ROW, false), cx);
            window.dispatch_event(down(ON_ROW), cx);
            window.dispatch_event(up(ON_ROW), cx);
        });
        vcx.run_until_parked();
    }

    #[gpui::test]
    fn a_release_disarms_so_a_later_hover_does_not_drag(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| Host);
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| {
            window.dispatch_event(moved(ON_ROW, false), cx);
            window.dispatch_event(down(ON_ROW), cx);
            window.dispatch_event(up(ON_ROW), cx);
        });
        vcx.update(|window, _| window.refresh());
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            window.dispatch_event(moved(drifted(ON_ROW), false), cx);
        });
        vcx.run_until_parked();
    }

    #[gpui::test]
    fn a_press_on_a_resize_handle_does_not_move_the_window(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| HandleOverRow { occluded: true });
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        press_repaint_move(&mut vcx, ON_ROW);
    }

    #[gpui::test]
    #[should_panic(expected = "not implemented")]
    fn a_handle_without_a_blocking_hitbox_hands_the_press_to_the_row(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, _| HandleOverRow { occluded: false });
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        press_repaint_move(&mut vcx, ON_ROW);
    }

    #[gpui::test]
    fn two_rows_on_screen_keep_separate_arms(cx: &mut TestAppContext) {
        struct TwoRows;
        impl Render for TwoRows {
            fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div()
                    .size_full()
                    .child(super::title_bar_drag(
                        div().id("row-a").w_full().h(px(40.)),
                        "row-a",
                        window,
                        cx,
                    ))
                    .child(super::title_bar_drag(
                        div().id("row-b").w_full().h(px(40.)),
                        "row-b",
                        window,
                        cx,
                    ))
            }
        }

        let window = cx.add_window(|_, _| TwoRows);
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| {
            window.dispatch_event(moved(point(px(300.), px(20.)), false), cx);
            window.dispatch_event(down(point(px(300.), px(20.))), cx);
        });
        vcx.update(|window, _| window.refresh());
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            window.dispatch_event(moved(point(px(312.), px(60.)), false), cx);
        });
        vcx.run_until_parked();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CloseReason, DOCUMENT_MIN_W, ForwardRoute, TERMINAL_MIN_W, TITLE_BAR_HEIGHT,
        TabAgentSession, clear_window_override_values, close_prompt, document_column_px,
        join_shell_args, leaf_shares_the_window_daemon, merge_typed_ssh_over_alias, mru_order,
        pane_free_for, parse_ssh_connect_input, parse_ssh_option_words, side_panel_max,
        split_shell_args, strip_band, wd_path_saveable,
    };
    use gpui::{Edges, point, px, size};

    /// A teardown that was never delivered must not answer the way a teardown
    /// that found nothing left answers.
    ///
    /// Nothing daemon-side reaps a workspace's forwards — only this request
    /// does — so "could not ask" is the case that leaks bound ports, and it
    /// used to come back as `Vec::new()`, i.e. indistinguishable from a clean
    /// teardown. Both routes here fail before any I/O, so neither needs a
    /// daemon to answer.
    #[test]
    fn an_undeliverable_workspace_teardown_is_not_an_empty_list() {
        let unaddressable = ForwardRoute {
            pane_id: 1,
            workspace: None,
        };
        assert!(
            unaddressable.teardown().is_none(),
            "a route with no workspace never asked, so it cannot report nothing left"
        );

        let no_spec = ForwardRoute {
            pane_id: 1,
            workspace: Some(crate::terminal::PaneWorkspace {
                workspace: crate::core::session::WorkspaceId::new(),
                target: crate::core::session::RemoteTarget::Alias {
                    alias: "somewhere".to_string(),
                },
                spec: None,
                resize_echo: false,
            }),
        };
        assert!(
            no_spec.teardown().is_none(),
            "a workspace with no ssh spec cannot be addressed either"
        );
    }

    const SIDEBAR_MIN: f32 = crate::ui::tab_sidebar::MIN_SIDEBAR_WIDTH;
    const PANEL_MIN: f32 = crate::ui::right_panel::MIN_WIDTH;

    /// #679: the band now starts at the frame padding rather than at the
    /// window's corner, and off Linux CSD there is no padding to start at —
    /// `window_paddings` answers `Edges::all(0)` under server-side decorations,
    /// which is what macOS, Windows and a bare X session all report.
    #[test]
    fn an_undecorated_frame_leaves_the_strips_drop_band_where_it_was() {
        let band = strip_band(size(px(1200.), px(800.)), Edges::all(px(0.)));
        assert_eq!(band.origin, point(px(0.), px(0.)));
        assert_eq!(band.size, size(px(1200.), px(TITLE_BAR_HEIGHT)));
    }

    /// The viewport measures the whole surface, shadow included, so the band
    /// has to lose one padding at each end — not one twice over, and not none.
    /// Getting it wrong puts the outermost chips outside the band drawn to hold
    /// them, and a drop on them reads as a drop on nothing.
    #[test]
    fn a_client_side_frame_pulls_the_drop_band_in_by_the_shadow_on_both_sides() {
        let viewport = size(px(1200.), px(800.));
        let pad = Edges::all(px(12.));
        let band = strip_band(viewport, pad);

        assert_eq!(band.origin, point(pad.left, pad.top));
        assert_eq!(
            band.origin.x + band.size.width,
            viewport.width - pad.right,
            "the band must reach the far edge of the frame, not of the surface"
        );
        assert_eq!(band.size.height, px(TITLE_BAR_HEIGHT));
    }

    /// A surface narrower than its own shadow is only reachable mid-resize, but
    /// a negative width would make `Bounds::contains` answer for a rectangle
    /// that is inside out.
    #[test]
    fn a_drop_band_narrower_than_its_frame_collapses_instead_of_inverting() {
        let band = strip_band(size(px(10.), px(800.)), Edges::all(px(12.)));
        assert_eq!(band.size.width, px(0.));
    }

    /// Two panels that each cap themselves at half the window leave the
    /// terminal nothing when both are open, so the cap is what is left after
    /// the terminal's floor and the *other* panel's floor — or half the window,
    /// whichever binds harder.
    #[test]
    fn the_terminal_is_spoken_for_before_either_panel_is() {
        // Mid width: the reservation binds, and a panel dragged to it with the
        // other at its floor leaves the terminal exactly its floor. Two halves
        // left it nothing.
        let mid = 900.;
        let sidebar = side_panel_max(mid, SIDEBAR_MIN, PANEL_MIN);
        assert_eq!(sidebar, mid - TERMINAL_MIN_W - PANEL_MIN);
        assert_eq!(mid - sidebar - PANEL_MIN, TERMINAL_MIN_W);

        // With nothing on the other side, half the window already leaves the
        // terminal the other half, so the older cap is the one that binds and
        // a lone panel behaves exactly as it always did.
        assert_eq!(side_panel_max(mid, SIDEBAR_MIN, 0.), mid / 2.);
    }

    /// A docked document is a third column in the same budget, so it has to be
    /// reserved by the two panels the way they already reserve each other —
    /// otherwise a panel dragged to its old limit takes the width out of the
    /// document, which then has nowhere to take it from but the terminal.
    #[test]
    fn a_docked_document_is_reserved_by_the_panels_too() {
        let wide = 1440.;
        let max = side_panel_max(wide, PANEL_MIN, SIDEBAR_MIN + DOCUMENT_MIN_W);
        assert_eq!(
            wide - SIDEBAR_MIN - DOCUMENT_MIN_W - max,
            TERMINAL_MIN_W,
            "a panel at its cap, with both other columns at their floors,              leaves the terminal exactly its floor"
        );
        assert!(
            max < side_panel_max(wide, PANEL_MIN, SIDEBAR_MIN),
            "the reservation only ever takes width away"
        );
    }

    /// The floors are not what the panels are actually drawn at. Both are
    /// draggable and both persist, so the budget has to be fed the live widths
    /// or a widened sidebar is width the terminal silently loses.
    #[test]
    fn widened_panels_still_leave_the_terminal_its_floor() {
        let viewport = 1440.;
        // Both dragged well past their floors, and the document asked for the
        // widest named share there is.
        let body = viewport - 400. - 320.;
        let document = document_column_px(body, 2. / 3.).expect("720 points seats both");
        assert!(
            body - document >= TERMINAL_MIN_W,
            "terminal got {}",
            body - document
        );

        // Squeezed further, the document is the one that gives up first — and
        // then stops existing rather than dropping under its own floor.
        let squeezed = viewport - 600. - 400.;
        assert_eq!(document_column_px(squeezed, 0.5), None);
    }

    /// The reservation must only ever take width away from a panel. On a wide
    /// window it works out *larger* than the half-window cap that was already
    /// there, and letting it win would widen the ceiling instead.
    #[test]
    fn a_wide_window_still_stops_a_panel_at_half_of_it() {
        let wide = 1440.;
        assert!(
            wide - TERMINAL_MIN_W - PANEL_MIN > wide / 2.,
            "otherwise this test is not testing the case it names"
        );
        assert_eq!(side_panel_max(wide, SIDEBAR_MIN, PANEL_MIN), wide / 2.);
        assert_eq!(side_panel_max(wide, PANEL_MIN, SIDEBAR_MIN), wide / 2.);
    }

    /// Below the width where everything fits, the floor wins over the
    /// reservation: a cap under a panel's own minimum would be a panel drawn
    /// narrower than it can be read at, and the terminal — which can reflow —
    /// takes the shortfall instead.
    #[test]
    fn a_window_too_narrow_for_all_three_falls_back_to_the_floors() {
        let narrow = 720.;
        assert_eq!(side_panel_max(narrow, SIDEBAR_MIN, PANEL_MIN), SIDEBAR_MIN);
        assert_eq!(side_panel_max(narrow, PANEL_MIN, SIDEBAR_MIN), PANEL_MIN);
        // Both panels pinned to their floors leaves the terminal the rest —
        // less than its floor, but more than the 260-odd points two saved
        // widths used to leave it.
        assert!(narrow - SIDEBAR_MIN - PANEL_MIN > 300.);
    }

    #[test]
    fn a_start_in_path_saves_only_when_it_names_a_real_directory() {
        // Empty is "unset", not a broken path.
        assert!(wd_path_saveable(""));
        assert!(wd_path_saveable("   "));
        let real = std::env::temp_dir();
        let real = real.to_str().expect("temp dir is utf-8 here");
        assert!(wd_path_saveable(real), "{real} exists");
        assert!(
            wd_path_saveable(&format!("  {real}  ")),
            "the commit trims, so the check trims too"
        );
        assert!(!wd_path_saveable("/definitely/not/a/real/dir"));
        // A file is not a directory the shell can start in.
        let file = std::env::temp_dir().join("tty7-wd-saveable-probe");
        std::fs::write(&file, b"x").expect("write probe file");
        assert!(!wd_path_saveable(file.to_str().expect("utf-8")));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn the_close_question_names_what_it_is_about_to_end() {
        use crate::terminal::view::PaneBusy;
        crate::ui::i18n::set_locale("en");

        let build = CloseReason::Busy(PaneBusy::Command("cargo build".into()));
        // The command is in the body, not a generic "are you sure".
        let (title, body) = close_prompt(true, &build);
        assert!(body.contains("cargo build"), "{body}");
        assert!(!body.contains("{what}"), "the placeholder leaked: {body}");
        assert!(title.contains("tab"), "{title}");
        // Same reason, but the pane has siblings: the tab survives, so the
        // question must not claim otherwise.
        let (pane_title, _) = close_prompt(false, &build);
        assert!(pane_title.contains("pane"), "{pane_title}");
        assert_ne!(title, pane_title);

        let agent = CloseReason::Busy(PaneBusy::Agent("Claude Code"));
        let (_, body) = close_prompt(true, &agent);
        assert!(body.contains("Claude Code"), "{body}");
        assert!(!body.contains("{agent}"), "the placeholder leaked: {body}");

        // A live SSH connection keeps its own wording rather than being folded
        // into the busy copy.
        let (ssh_title, ssh_body) = close_prompt(true, &CloseReason::LiveSsh);
        assert_ne!(ssh_title, title);
        assert!(!ssh_body.is_empty());
    }

    #[test]
    fn a_layout_that_came_back_short_says_how_short() {
        crate::ui::i18n::set_locale("en");
        for n in [1usize, 2, 9] {
            let text =
                crate::ui::i18n::t_plural(crate::ui::i18n::L10nKey::AppTabsNotRestored, n, &[]);
            assert!(text.contains(&n.to_string()), "{n}: {text}");
            assert!(!text.contains("{count}"), "the placeholder leaked: {text}");
        }
        // Singular is not "1 tabs".
        let one = crate::ui::i18n::t_plural(crate::ui::i18n::L10nKey::AppTabsNotRestored, 1, &[]);
        assert!(one.contains("1 tab "), "{one}");
    }

    #[test]
    fn non_windows_reset_preserves_the_synced_windows_backdrop() {
        let mut config = crate::core::config::Config::default();
        config.window_opacity = Some(0.8);
        config.window_blur = Some(true);
        config.window_backdrop = crate::core::config::WindowBackdrop::Mica;

        clear_window_override_values(&mut config, false);

        assert_eq!(config.window_opacity, None);
        assert_eq!(config.window_blur, None);
        assert_eq!(
            config.window_backdrop,
            crate::core::config::WindowBackdrop::Mica,
            "an inert synchronized backdrop is not a local override to reset"
        );
    }

    #[test]
    fn windows_reset_clears_the_local_backdrop_override() {
        let mut config = crate::core::config::Config::default();
        config.window_backdrop = crate::core::config::WindowBackdrop::Acrylic;

        clear_window_override_values(&mut config, true);

        assert_eq!(
            config.window_backdrop,
            crate::core::config::WindowBackdrop::Auto
        );
    }

    #[test]
    fn mru_puts_the_active_tab_first_and_the_last_one_used_behind_it() {
        // Tab 2 is active; 0 was used most recently before it, then 1.
        assert_eq!(mru_order(&[7, 4, 9], 2), vec![2, 0, 1]);
    }

    #[test]
    fn mru_leads_with_the_active_tab_even_before_its_stamp_lands() {
        assert_eq!(mru_order(&[5, 0, 3], 1), vec![1, 0, 2]);
    }

    #[test]
    fn mru_trails_never_activated_tabs_in_strip_order() {
        assert_eq!(mru_order(&[0, 6, 0, 0], 1), vec![1, 0, 2, 3]);
    }

    #[test]
    fn mru_of_a_windowless_workspace_is_empty() {
        assert!(mru_order(&[], 0).is_empty());
    }

    #[test]
    fn restore_only_attaches_panes_the_workspace_owns_or_nobody_claims() {
        let ours = crate::core::session::WorkspaceId::new();
        let theirs = crate::core::session::WorkspaceId::new();
        let alive: std::collections::HashMap<u64, Option<String>> = [
            (1, Some(ours.to_string())),
            (2, Some(theirs.to_string())),
            (3, None),
            (5, Some("tty7-cli".to_string())),
        ]
        .into_iter()
        .collect();

        assert!(
            pane_free_for(Some(&alive), 1, ours),
            "our own pane attaches"
        );
        assert!(
            !pane_free_for(Some(&alive), 2, ours),
            "another workspace's pane must spawn fresh instead"
        );
        assert!(
            pane_free_for(Some(&alive), 3, ours),
            "an unowned pane is legacy"
        );
        assert!(
            pane_free_for(Some(&alive), 5, ours),
            "an owner that names no workspace is not a rival's claim: older CLIs \
             wrote their own name there, and respawning strands the live pane"
        );
        assert!(
            pane_free_for(None, 4, ours),
            "a failed List says nothing about pane 4; the attach itself must decide, \
             because respawning on a transient RPC error destroys a live session"
        );
    }

    #[test]
    fn a_dead_pane_keeps_its_id_so_its_screen_can_be_asked_for() {
        let ours = crate::core::session::WorkspaceId::new();
        let theirs = crate::core::session::WorkspaceId::new();
        let alive: std::collections::HashMap<u64, Option<String>> =
            [(1, Some(ours.to_string())), (2, Some(theirs.to_string()))]
                .into_iter()
                .collect();

        // The restart case: every pane the window held is missing from the new
        // daemon's listing. Their ids are the only handle on the screens it
        // still has stored, so being dead must not erase them — this is what
        // made a restarted server come back to a row of blank shells.
        assert!(
            pane_free_for(Some(&alive), 4, ours),
            "a dead pane's id has to survive; the restore is keyed on it"
        );

        // What being free does not mean: helping yourself to a pane that is
        // alive and belongs to another workspace, whose screen is not this
        // window's to show either.
        assert!(
            !pane_free_for(Some(&alive), 2, ours),
            "another workspace's pane is not ours to restore from"
        );
        assert!(pane_free_for(Some(&alive), 1, ours), "our own pane is ours");
    }

    #[test]
    fn a_native_ssh_leaf_in_a_remote_window_is_not_looked_up_in_the_remote_daemon() {
        assert!(!leaf_shares_the_window_daemon(true, true));
        assert!(leaf_shares_the_window_daemon(true, false));
        assert!(leaf_shares_the_window_daemon(false, true));
        assert!(leaf_shares_the_window_daemon(false, false));
    }

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

    #[gpui::test]
    fn a_connecting_pane_saves_the_agent_it_is_rebuilding(cx: &mut gpui::TestAppContext) {
        use crate::core::cli_agent::CLIAgent;
        use crate::core::session::SessionPane;
        use crate::ui::pane::{Pane, PaneSlot};
        use crate::ui::pending_pane::{PendingPane, PendingSpawn};
        use gpui::AppContext as _;

        cx.update(|cx| {
            let pending = cx.new(|cx| {
                PendingPane::new(
                    "build-box",
                    PendingSpawn {
                        workspace: None,
                        working_directory: Some(std::path::PathBuf::from("/work")),
                        restore_pane: Some(7),
                        shell: None,
                        agent: Some(CLIAgent::Claude),
                        agent_session_id: Some("sid-abc".to_string()),
                        agent_launch_argv: Some(vec!["claude".to_string()]),
                        owner: None,
                        font_size: 14.0,
                    },
                    cx,
                )
            });
            let saved = super::pane_to_session(&Pane::leaf(PaneSlot::Connecting(pending)), cx);
            let SessionPane::Leaf {
                pane_id,
                agent,
                agent_session_id,
                agent_launch_argv,
                ..
            } = saved
            else {
                panic!("a leaf saves as a leaf");
            };
            assert_eq!(pane_id, Some(7), "the id it is re-attaching to");
            assert_eq!(agent, Some(CLIAgent::Claude));
            assert_eq!(agent_session_id.as_deref(), Some("sid-abc"));
            assert_eq!(agent_launch_argv, Some(vec!["claude".to_string()]));
        });
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
        let p = parse_ssh_connect_input("ssh deploy@10.0.0.5:2222").unwrap();
        assert_eq!(p.profile.host, "10.0.0.5");
        assert_eq!(p.profile.user, "deploy");
        assert_eq!(p.profile.port, 2222);
        assert!(p.proxy_jump.is_none());
    }

    #[test]
    fn parses_typed_connect_flags_and_jump() {
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

        let p = parse_ssh_connect_input("host -p2222 -o User=deploy -o Port=2200").unwrap();
        assert_eq!(p.profile.user, "deploy");
        assert_eq!(p.profile.port, 2200);
    }

    #[test]
    fn explicit_flags_override_target_userhost() {
        let p = parse_ssh_connect_input("ssh me@host:22 -l other -p 2200").unwrap();
        assert_eq!(p.profile.user, "other");
        assert_eq!(p.profile.port, 2200);
    }

    #[test]
    fn a_typed_port_of_22_still_outranks_an_alias_on_another_port() {
        use crate::core::ssh_config::ResolvedAlias;
        use crate::core::ssh_profile::SshProfile;

        // `ssh -G -p 22 myalias` reports port 22 even where the alias sets
        // 2222, checked against the real ssh. Port 22 doubles as the default,
        // so asking `profile.port != 22` read the explicit request as silence
        // and connected to the alias's port instead.
        let alias = || {
            let mut a = SshProfile::new("myalias".to_string());
            a.host = "example.com".to_string();
            a.user = "alice".to_string();
            a.port = 2222;
            Some(ResolvedAlias {
                profile: a,
                proxy_jump: None,
            })
        };

        let typed = parse_ssh_connect_input("myalias -p 22").unwrap();
        assert!(typed.port_given);
        let (merged, _) = merge_typed_ssh_over_alias(typed, alias());
        assert_eq!(merged.port, 22, "a typed -p 22 was ignored");
        assert_eq!(merged.user, "alice", "the alias still supplies the user");

        // The `:port` spelling of the same request.
        let typed = parse_ssh_connect_input("myalias:22").unwrap();
        assert!(typed.port_given);
        assert_eq!(merge_typed_ssh_over_alias(typed, alias()).0.port, 22);

        // Saying nothing about the port still leaves the alias in charge.
        let typed = parse_ssh_connect_input("myalias").unwrap();
        assert!(!typed.port_given);
        assert_eq!(merge_typed_ssh_over_alias(typed, alias()).0.port, 2222);

        // And a typed port that is not 22 was never in doubt.
        let typed = parse_ssh_connect_input("myalias -p 2200").unwrap();
        assert_eq!(merge_typed_ssh_over_alias(typed, alias()).0.port, 2200);

        // With no alias at all the typed line stands on its own.
        let typed = parse_ssh_connect_input("myalias -p 22").unwrap();
        assert_eq!(merge_typed_ssh_over_alias(typed, None).0.port, 22);
    }

    #[test]
    fn rejects_bad_typed_connect_lines() {
        assert!(parse_ssh_connect_input("ssh -p 2222").is_err());
        assert!(parse_ssh_connect_input("ssh dev uptime").is_err());
        assert!(parse_ssh_connect_input("ssh -- dev").is_err());
        assert!(parse_ssh_connect_input("ssh 'host").is_err());
        assert!(parse_ssh_connect_input("ssh host -p 0").is_err());
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    /// The invariant #551 is fixed with: what `build_shell_inputs` writes into
    /// the Arguments field must parse back to exactly the argv it came from, so
    /// that opening Settings and clicking away cannot rewrite `config.json`.
    #[test]
    fn shell_arguments_round_trip_between_field_text_and_argv() {
        for case in [
            &["-l"][..],
            &["-c", "echo hi"],
            // A space-join cannot even spell these three.
            &[""],
            &["don't"],
            &["--say", "it's \"quoted\""],
            // Backslashes are how a path is spelled on Windows, and the field
            // has to be able to carry one either bare or inside quotes.
            &[r"--dir", r"C:\Users\me"],
            &[r"--dir", r"C:\Program Files\tty7"],
            &["--tag", "#1"],
        ] {
            let argv = argv(case);
            let field_text = join_shell_args(&argv);
            assert_eq!(
                split_shell_args(&field_text).expect("tty7's own refill must parse"),
                argv,
                "round trip through {field_text:?}"
            );
        }
        // Nothing that does not need quoting acquires any: the everyday field
        // still reads exactly like the user typed it.
        assert_eq!(
            join_shell_args(&argv(&["-l", "--color=auto", r"C:\Users\me", "#1"])),
            r"-l --color=auto C:\Users\me #1"
        );
        assert_eq!(join_shell_args(&[]), "");
    }

    /// What the user types has to reach argv meaning what it says: quoted text
    /// is one argument, and nothing outside quotes is an escape or a comment.
    #[test]
    fn shell_arguments_split_the_way_the_field_promises() {
        assert_eq!(
            split_shell_args("--login -c \"echo hi\"").expect("balanced quotes parse"),
            argv(&["--login", "-c", "echo hi"])
        );
        assert_eq!(
            split_shell_args("-c 'echo  hi'").expect("single quotes parse"),
            argv(&["-c", "echo  hi"])
        );
        // A backslash outside quotes is a character, not an escape: there is no
        // shell between this field and argv, and POSIX rules would quietly turn
        // a Windows path into `C:Usersme`.
        assert_eq!(
            split_shell_args(r"--dir C:\Users\me").expect("a bare path parses"),
            argv(&["--dir", r"C:\Users\me"])
        );
        // `#` starts no comment either — swallowing the rest of the line is the
        // silent rewrite this change exists to stop.
        assert_eq!(
            split_shell_args("--tag #1 --verbose").expect("a hash parses"),
            argv(&["--tag", "#1", "--verbose"])
        );
        // Inside double quotes, and only there, `\"` and `\\` stand for
        // themselves — that is how `join_shell_args` spells an argument that
        // carries both kinds of quote.
        assert_eq!(
            split_shell_args("\"it's \\\"quoted\\\"\"").expect("escapes parse"),
            argv(&["it's \"quoted\""])
        );
        assert_eq!(
            split_shell_args("'' -l").expect("an empty argument parses"),
            argv(&["", "-l"])
        );
    }

    /// The other half of the #551 contract: a value that cannot become argv is
    /// refused, not shredded into fragments that happen to contain quotes.
    #[test]
    fn unbalanced_quotes_are_refused_not_shredded() {
        assert!(split_shell_args("-c \"echo hi").is_err());
        assert!(split_shell_args("--name 'tty7").is_err());
        assert!(split_shell_args("\"a\\\"").is_err());
    }
}

#[cfg(test)]
pub(crate) mod test_window {
    use crate::core::config::Config;
    use crate::core::session::Session;
    use crate::ui::app::Tty7App;
    use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};

    pub(crate) fn harness(cx: &mut TestAppContext) -> (Entity<Tty7App>, VisualTestContext) {
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
        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.background_executor.run_until_parked();
        let app = window
            .update(cx, |root, _, _| {
                root.view()
                    .clone()
                    .downcast::<Tty7App>()
                    .expect("window root wraps a Tty7App")
            })
            .unwrap();
        let vcx = VisualTestContext::from_window(window.into(), cx);
        (app, vcx)
    }

    /// A window carrying `n` quiet tabs, active on the first.
    #[cfg(unix)]
    pub(crate) fn harness_with_tabs(
        cx: &mut TestAppContext,
        n: usize,
    ) -> (
        Entity<Tty7App>,
        VisualTestContext,
        Vec<std::os::unix::net::UnixStream>,
    ) {
        use crate::terminal::view::quiet_test_pane;
        use crate::ui::pane::{Pane, PaneSlot};

        let (app, mut vcx) = harness(cx);
        vcx.update(|_, cx| {
            let mut cfg = cx.global::<Config>().clone();
            cfg.cursor_blink = false;
            cx.set_global(cfg);
        });
        let streams = app.update_in(&mut vcx, |app, window, cx| {
            let mut streams = Vec::new();
            for i in 0..n {
                let (view, stream) = quiet_test_pane(i as u64 + 1, window, cx);
                app.tabs
                    .push(super::Tab::new(Pane::leaf(PaneSlot::Ready(view))));
                streams.push(stream);
            }
            app.active = 0;
            cx.notify();
            streams
        });
        vcx.background_executor.run_until_parked();
        (app, vcx, streams)
    }

    #[cfg(unix)]
    pub(crate) fn harness_with_pane(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Tty7App>,
        VisualTestContext,
        std::os::unix::net::UnixStream,
    ) {
        use crate::terminal::view::quiet_test_pane;
        use crate::ui::pane::{Pane, PaneSlot};

        let (app, mut vcx) = harness(cx);
        vcx.update(|_, cx| {
            let mut cfg = cx.global::<Config>().clone();
            cfg.cursor_blink = false;
            cx.set_global(cfg);
        });
        let stream = app.update_in(&mut vcx, |app, window, cx| {
            let (view, stream) = quiet_test_pane(1, window, cx);
            app.tabs
                .push(super::Tab::new(Pane::leaf(PaneSlot::Ready(view))));
            app.active = 0;
            cx.notify();
            stream
        });
        vcx.background_executor.run_until_parked();
        (app, vcx, stream)
    }

    /// Wait until the window has actually stopped drawing — which is not the
    /// same as having reached the state a test was waiting for.
    ///
    /// Called both after a settle and at the top of `draws_while_idle`: a test
    /// that reaches its state, asserts a few things about it and only then
    /// measures has given the setup more time to land, but not necessarily
    /// enough, and the measurement is the place that cannot afford to be
    /// wrong.
    ///
    /// Both of a `render_idle` test's clocks have to be pumped here, and they
    /// are pumped differently.
    ///
    /// The pane runs its own git pipeline, separate from whatever panel is on
    /// screen, and it hangs off a 300ms timer on the *virtual* clock — so it
    /// never starts at all unless a test advances that clock. It used to be
    /// `draws_while_idle`'s own `advance_clock` that started it, which put the
    /// pane's first real `git` run, and the repaint it lands with, inside the
    /// window being counted. Whether that repaint arrived before or after the
    /// count then came down to how fast git ran, which is why these tests were
    /// green here and red on a loaded CI runner (issue #523).
    ///
    /// What that repaint sets off in turn is timed on the *real* clock: the
    /// landing opens a `GIT_WATCH_DEBOUNCE` burst, and closing the burst costs
    /// another frame 250ms later. So the sleep below is load-bearing too, and
    /// a round that drew nothing is not on its own enough to stop on — a burst
    /// still open is a frame already owed.
    #[cfg(unix)]
    pub(crate) fn quiesce(vcx: &mut VisualTestContext, cwd: Option<&std::path::Path>) {
        use crate::terminal::git_data::ScmData;
        use crate::terminal::git_status::GitStatusCache;
        use crate::ui::app::render_probe;
        use crate::ui::host_ops::HostId;

        /// How long quiet has to hold before it counts as quiet.
        ///
        /// Real time, and the only defence against the third clock in play:
        /// the kernel's. The file tree keeps a real `inotify`/`FSEvents`
        /// watch, and the writes a test makes while setting up its repository
        /// are still being delivered long after every future the test can wait
        /// on has resolved. They arrive on the channel, sit in a 200ms debounce
        /// on the virtual clock, and are released by the next `advance_clock`
        /// — which, without this, was the measurement's own. Any delivery
        /// restarts the hold, so the wait is as long as the runner needs and
        /// no longer.
        const QUIET_HOLD: std::time::Duration = std::time::Duration::from_millis(400);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut quiet_since: Option<std::time::Instant> = None;
        loop {
            render_probe::arm(u64::MAX);
            vcx.executor()
                .advance_clock(std::time::Duration::from_millis(300));
            vcx.background_executor.run_until_parked();
            let quiet = render_probe::draws() == 0
                && vcx.update(|_, cx| {
                    let owed = cx
                        .try_global::<ScmData>()
                        .is_some_and(ScmData::is_debouncing);
                    let answered = cwd.is_none_or(|cwd| {
                        cx.try_global::<GitStatusCache>()
                            .and_then(|cache| cache.known_repo_for(HostId::LOCAL, cwd))
                            .is_some()
                    });
                    !owed && answered
                });
            match quiet {
                false => quiet_since = None,
                true => {
                    let since = *quiet_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() >= QUIET_HOLD {
                        return;
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the window never stopped drawing"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

#[cfg(all(test, unix))]
mod ssh_rebuild_gpui_tests {
    use super::test_window::harness_with_pane;
    use crate::core::session::{
        RemoteRef, RemoteTarget, WindowView, WindowViews, WorkspaceId, WorkspaceStore,
    };
    use crate::ui::pane::{Pane, PaneSlot};
    use gpui::TestAppContext;
    use tty7_core::core::machine::{LayoutDelta, PaneNode, Tab as TreeTab};

    #[gpui::test]
    fn a_tree_rebuild_keeps_the_native_ssh_split_a_remote_tab_holds(cx: &mut TestAppContext) {
        let (app, mut vcx, _remote_pane_stream) = harness_with_pane(cx);

        let remote = WindowView::on_remote(RemoteRef::new(
            RemoteTarget::Alias {
                alias: "build-box".into(),
            },
            WorkspaceId::new(),
        ));
        let remote_id = remote.id;
        let _ssh_stream = app.update_in(&mut vcx, |app, window, cx| {
            WorkspaceStore::install_for_test(
                cx,
                WindowViews {
                    views: vec![remote],
                    active: None,
                },
            );
            app.workspace = remote_id;
            let (ssh_view, stream) = crate::terminal::view::quiet_test_ssh_pane(2, window, cx);
            let existing = std::mem::replace(&mut app.tabs[0].pane, Pane::Empty);
            app.tabs[0].pane = Pane::split_node(
                gpui::Axis::Horizontal,
                0.5,
                existing,
                Pane::leaf(PaneSlot::Ready(ssh_view)),
            );
            stream
        });

        let applied = app.update_in(&mut vcx, |app, window, cx| {
            let tab = TreeTab {
                id: app.tabs[0].tree_id.get(),
                name: None,
                sidebar_group: None,
                root: PaneNode::Leaf { pane: 1 },
            };
            app.apply_layout_delta(
                &LayoutDelta::TabRestructured { tab, pane: None },
                window,
                cx,
            )
        });
        assert!(
            applied,
            "the delta must apply without falling back to a resync"
        );

        app.update_in(&mut vcx, |app, _, cx| {
            let leaves = app.tabs[0].pane.leaves();
            assert_eq!(leaves.len(), 2, "the ssh split must survive the rebuild");
            assert!(
                leaves.iter().any(|slot| match slot {
                    PaneSlot::Ready(view) => view.read(cx).ssh_spec().is_some(),
                    _ => false,
                }),
                "one leaf is still the native-SSH pane"
            );
            assert!(
                leaves.iter().any(|slot| match slot {
                    PaneSlot::Ready(view) => {
                        let view = view.read(cx);
                        view.ssh_spec().is_none() && view.pane_id == 1
                    }
                    _ => false,
                }),
                "the remote pane's existing view is reused, not re-attached"
            );
        });
    }

    #[gpui::test]
    fn a_pure_native_ssh_tab_is_invisible_to_the_tree_not_held(cx: &mut TestAppContext) {
        let (app, mut vcx, _remote_pane_stream) = harness_with_pane(cx);

        let remote = WindowView::on_remote(RemoteRef::new(
            RemoteTarget::Alias {
                alias: "build-box".into(),
            },
            WorkspaceId::new(),
        ));
        let remote_id = remote.id;
        let _ssh_stream = app.update_in(&mut vcx, |app, window, cx| {
            WorkspaceStore::install_for_test(
                cx,
                WindowViews {
                    views: vec![remote],
                    active: None,
                },
            );
            app.workspace = remote_id;
            let (ssh_view, stream) = crate::terminal::view::quiet_test_ssh_pane(2, window, cx);
            app.tabs
                .push(super::Tab::new(Pane::leaf(PaneSlot::Ready(ssh_view))));
            stream
        });

        let (desired, _active, held) = app.update_in(&mut vcx, |app, _, cx| {
            crate::ui::tree_sync::desired_tabs(app, cx)
        });
        assert_eq!(
            desired.len(),
            1,
            "only the remote-backed tab can be named in the machine's tree"
        );
        assert!(
            held.is_empty(),
            "the pure-SSH tab is permanently invisible, not held — holding it \
             would freeze ordering and active-tab sync for the whole window"
        );
    }
}

#[cfg(test)]
mod keybinding_gpui_tests {
    use super::test_window::harness;
    use crate::core::config::Config;
    use crate::ui::app::Tty7App;
    use crate::ui::settings::SettingsSection;
    use gpui::{Entity, TestAppContext, VisualTestContext};

    fn begin_capture(app: &Entity<Tty7App>, vcx: &mut VisualTestContext, action: &str) {
        let action = action.to_string();
        app.update_in(vcx, |app, window, cx| {
            app.toggle_settings(window, cx);
            app.select_settings_section(SettingsSection::Keybindings, cx);
            app.start_recording_key(action, window, cx);
        });
    }

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

    #[gpui::test]
    fn recording_a_shortcut_writes_the_override_and_ends_capture(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);
        begin_capture(&app, &mut vcx, "NewTab");
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

    #[gpui::test]
    fn recording_a_two_chord_sequence_writes_the_full_spec(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);
        begin_capture(&app, &mut vcx, "CloseActiveTab");
        vcx.simulate_keystrokes("secondary-b");
        vcx.simulate_keystrokes("x");
        wait_for_binding(&mut vcx, "CloseActiveTab", "secondary-b x");
    }

    #[gpui::test]
    fn recording_an_extra_default_chord_displaces_its_owner(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);
        begin_capture(&app, &mut vcx, "NewTab");
        vcx.simulate_keystrokes("alt-enter");
        wait_for_binding(&mut vcx, "NewTab", "alt-enter");
        wait_for_binding(&mut vcx, "InsertNewline", "");

        let note = app.update_in(&mut vcx, |app, _, _| {
            app.active_settings().and_then(|s| s.rebinding_note.clone())
        });
        assert!(
            note.as_deref()
                .is_some_and(|n| n.contains("Insert Newline")),
            "the takeover note must name the action that lost the chord (got {note:?})"
        );
    }

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

#[cfg(test)]
mod restart_server_gpui_tests {
    use crate::core::config::Config;
    use crate::core::session::Session;
    use crate::ui::app::Tty7App;
    use gpui::{AppContext, TestAppContext};

    /// Clicking Restart Server made the whole app disappear.
    ///
    /// The work that puts the window back together after the restart ends by
    /// rebuilding every local window from the machine tree, and the first thing
    /// that rebuild asks each window is which tabs it is showing — which it
    /// reads back out of the window registry. Run from inside `update_in` on
    /// this window's own entity, the first window it reaches for is the one the
    /// closure already holds leased, and gpui answers a double lease by
    /// panicking, which on the main thread is the process.
    ///
    /// Driven through `settle_after_restart` with a restart that "succeeded",
    /// because the crash is in the part that runs either way, not in the
    /// restart itself.
    #[gpui::test]
    async fn settling_after_a_restart_does_not_lease_the_window_twice(cx: &mut TestAppContext) {
        crate::core::config::pin_test_config_dir();
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            crate::ui::keymap::init(cx);
            crate::ui::windows::WindowRegistry::init(cx);
        });
        let window = cx.add_window(|window, cx| {
            let app =
                cx.new(|cx| Tty7App::with_session(None, Some(Session::default()), window, cx));
            gpui_component::Root::new(app, window, cx)
        });
        let app = window
            .update(cx, |root, _, _| {
                root.view()
                    .clone()
                    .downcast::<Tty7App>()
                    .expect("window root wraps a Tty7App")
            })
            .unwrap();

        // Registered the way an opened window registers itself: without this
        // the rebuild finds no window to ask and never reaches for the entity,
        // which is the whole thing under test.
        let handle = window.into();
        let weak = app.downgrade();
        app.update(cx, |app, cx| {
            crate::ui::windows::WindowRegistry::register(cx, app.workspace, handle, weak);
        });

        Tty7App::settle_after_restart(app.downgrade(), Ok(()), &mut cx.to_async()).await;

        assert!(
            app.update(cx, |app, _| app.startup_error.is_none()),
            "a restart reported as successful must not leave an error banner"
        );
    }
}

#[cfg(test)]
mod shell_menu_gpui_tests {
    use crate::core::config::Config;
    use crate::core::session::{
        RemoteRef, RemoteTarget, Session, WindowView, WindowViews, WorkspaceId, WorkspaceStore,
    };
    use crate::ui::app::Tty7App;
    use gpui::{AppContext, Entity, TestAppContext, VisualTestContext};

    fn harness(cx: &mut TestAppContext) -> (Entity<Tty7App>, VisualTestContext) {
        crate::core::config::pin_test_config_dir();
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(Config::default());
            crate::ui::keymap::init(cx);
            crate::ui::windows::WindowRegistry::init(cx);
        });
        let window = cx.add_window(|window, cx| {
            let app =
                cx.new(|cx| Tty7App::with_session(None, Some(Session::default()), window, cx));
            gpui_component::Root::new(app, window, cx)
        });
        let app = window
            .update(cx, |root, _, _| {
                root.view()
                    .clone()
                    .downcast::<Tty7App>()
                    .expect("window root wraps a Tty7App")
            })
            .unwrap();
        let vcx = VisualTestContext::from_window(window.into(), cx);
        (app, vcx)
    }

    fn pump_until(
        app: &Entity<Tty7App>,
        vcx: &mut VisualTestContext,
        done: impl Fn(&Tty7App) -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            vcx.background_executor.run_until_parked();
            if app.update(vcx, |app, _| done(app)) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[gpui::test]
    fn a_local_window_lists_this_computers_shells(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);
        assert!(
            pump_until(&app, &mut vcx, |app| !app.shells.shells.is_empty()),
            "the local probe never landed"
        );
        app.update(&mut vcx, |app, _| {
            assert!(app.shells_host.is_local());
            assert!(
                !app.shells.default_name.is_empty(),
                "the menu has no default to tag"
            );
        });
    }

    #[gpui::test]
    fn an_unreachable_remote_window_offers_no_local_shells(cx: &mut TestAppContext) {
        let (app, mut vcx) = harness(cx);
        assert!(
            pump_until(&app, &mut vcx, |app| !app.shells.shells.is_empty()),
            "the local probe never landed"
        );

        let remote = WindowView::on_remote(RemoteRef::new(
            RemoteTarget::Alias {
                alias: "build-box".into(),
            },
            WorkspaceId::new(),
        ));
        let remote_id = remote.id;
        app.update_in(&mut vcx, |app, window, cx| {
            WorkspaceStore::install_for_test(
                cx,
                WindowViews {
                    views: vec![remote],
                    active: None,
                },
            );
            app.switch_workspace(Some(remote_id), window, cx);
        });

        assert!(
            pump_until(&app, &mut vcx, |app| !app.shells_host.is_local()),
            "the window never rebound to the remote machine"
        );
        app.update(&mut vcx, |app, _| {
            assert!(
                app.shells.shells.is_empty(),
                "a remote window must not offer this computer's shells: {:?}",
                app.shells.shells
            );
        });
    }
}

// `harness_with_tabs` hands back the panes' `UnixStream`s, so it exists only
// on unix — same as `ssh_rebuild_gpui_tests` below it.
#[cfg(all(test, unix))]
mod rename_gpui_tests {
    use gpui::TestAppContext;

    use crate::ui::app::test_window::harness_with_tabs;

    #[gpui::test]
    fn a_rename_box_opens_with_the_caret_after_the_name(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        app.update_in(&mut vcx, |app, window, cx| app.start_rename(0, window, cx));
        vcx.background_executor.run_until_parked();

        app.update(&mut vcx, |app, cx| {
            let input = app
                .renaming
                .as_ref()
                .expect("the rename box is up")
                .input
                .clone();
            let state = input.read(cx);
            let value = state.value().to_string();
            assert!(!value.is_empty(), "the box starts on the current name");
            let end = value.len();
            assert_eq!(
                state.selected_range(),
                end..end,
                "typing has to continue {value:?}, not land in front of it"
            );
        });
    }
    #[gpui::test]
    fn a_rename_rides_out_other_tabs_closing_and_the_strip_reordering(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 3);

        app.update_in(&mut vcx, |app, window, cx| {
            app.start_rename(2, window, cx);
            let target = app.tabs[2].tree_id.get();
            let input = app.renaming.as_ref().expect("the box is up").input.clone();
            input.update(cx, |s, cx| s.set_value("mine", window, cx));

            // An unrelated close used to throw the half-typed name away
            // (#598).
            app.close_tab_inner(0, true, window, cx);
            assert!(
                app.renaming.is_some(),
                "closing another tab keeps the rename box"
            );

            // So did a drag-reorder — and the index the commit once landed
            // on had by then drifted onto a different tab.
            let order: Vec<usize> = (0..app.tabs.len()).rev().collect();
            app.apply_tab_order(&order, cx);
            assert!(app.renaming.is_some(), "a reorder keeps the rename box");

            app.commit_rename(window, cx);
            let named: Vec<_> = app
                .tabs
                .iter()
                .filter(|t| t.name.as_deref() == Some("mine"))
                .collect();
            assert_eq!(named.len(), 1, "the name landed on exactly one tab");
            assert_eq!(
                named[0].tree_id.get(),
                target,
                "and that tab is the one the box was opened on"
            );
        });

        // Closing the renaming tab itself still ends the rename.
        app.update_in(&mut vcx, |app, window, cx| {
            app.start_rename(0, window, cx);
            assert!(app.renaming.is_some());
            app.close_tab_inner(0, true, window, cx);
            assert!(app.renaming.is_none(), "losing its own tab closes the box");
        });
    }
}

// The output gate: a pane repaints on PTY output only while it is on screen.
// `Tty7App::render` declares the active tab's panes displayed each frame and
// everything else hidden; a pane nobody has declared — or whose id nobody
// registered — must err toward displayed, because the failure direction that
// matters is a visible pane that stops repainting.
#[cfg(all(test, unix))]
mod displayed_gpui_tests {
    use gpui::TestAppContext;

    use crate::terminal::view::{declare_displayed, displayed_for_test, quiet_test_pane};
    use crate::ui::app::test_window::harness_with_tabs;

    fn pane_id(app: &super::Tty7App, tab: usize) -> gpui::EntityId {
        app.tabs[tab]
            .pane
            .first_leaf()
            .expect("tab has a pane")
            .entity_id()
    }

    #[gpui::test]
    fn the_output_gate_follows_the_active_tab(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);

        app.update_in(&mut vcx, |app, window, cx| {
            let (front, back) = (pane_id(app, 0), pane_id(app, 1));

            app.declare_displayed_panes(cx);
            assert_eq!(
                displayed_for_test(cx, front),
                Some(true),
                "the active tab's pane repaints on output"
            );
            assert_eq!(
                displayed_for_test(cx, back),
                Some(false),
                "a background tab's pane stays out of the frame loop"
            );

            app.activate(1, window, cx);
            app.declare_displayed_panes(cx);
            assert_eq!(displayed_for_test(cx, front), Some(false));
            assert_eq!(
                displayed_for_test(cx, back),
                Some(true),
                "switching tabs hands the frame loop to the new tab"
            );
        });
    }

    #[gpui::test]
    fn a_pane_nobody_declared_counts_as_displayed(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        let _orphan = app.update_in(&mut vcx, |app, window, cx| {
            // A pane that exists but sits in no tab — the shape of every
            // "path that never declares": it registers as displayed and a
            // declaration pass over the app's own tabs leaves it alone.
            let (view, stream) = quiet_test_pane(99, window, cx);
            let orphan = view.entity_id();
            assert_eq!(
                displayed_for_test(cx, orphan),
                Some(true),
                "a fresh pane defaults to displayed"
            );

            app.declare_displayed_panes(cx);
            assert_eq!(
                displayed_for_test(cx, orphan),
                Some(true),
                "declaring only speaks about panes the app holds"
            );

            // An id nobody registered is declared into the void, not
            // inserted: the registry only ever holds live panes' flags.
            let ghost = gpui::EntityId::from(u64::MAX);
            declare_displayed(cx, [(ghost, false)]);
            assert_eq!(displayed_for_test(cx, ghost), None);

            (view, stream)
        });
    }

    #[gpui::test]
    fn a_released_pane_leaves_the_registry(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);

        let back = app.update_in(&mut vcx, |app, _window, _cx| {
            let back = pane_id(app, 1);
            app.tabs.remove(1);
            back
        });
        vcx.background_executor.run_until_parked();

        app.update_in(&mut vcx, |_, _, cx| {
            assert_eq!(
                displayed_for_test(cx, back),
                None,
                "a closed pane's flag does not outlive it"
            );
        });
    }
}

// Zoom is a tab's view state: it rides with the tab across a switch, while a
// layout change (drag, split, close) still clears it.
#[cfg(all(test, unix))]
mod zoom_gpui_tests {
    use gpui::TestAppContext;

    use crate::ui::app::test_window::harness_with_tabs;

    #[gpui::test]
    fn a_tabs_zoom_survives_a_round_trip_to_another_tab(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 2);

        app.update_in(&mut vcx, |app, window, cx| {
            let leaf = app.tabs[0]
                .pane
                .first_leaf()
                .and_then(|slot| slot.terminal().cloned())
                .expect("tab 0 has a pane");
            app.maximized = Some(leaf.clone());

            app.activate(1, window, cx);
            assert!(app.maximized.is_none(), "tab 1 never zoomed anything");

            app.activate(0, window, cx);
            assert_eq!(
                app.maximized.as_ref().map(|l| l.entity_id()),
                Some(leaf.entity_id()),
                "tab 0's zoom is still where it was left (#599)"
            );

            // A zoom stashed for a pane that is no longer in the tab does not
            // come back — it exited (or was closed) while the tab was away.
            app.maximized = Some(leaf.clone());
            app.activate(1, window, cx);
            app.tabs[0].zoomed = Some(leaf);
            app.tabs[0].pane =
                crate::ui::pane::Pane::leaf(app.tabs[1].pane.first_leaf().expect("a donor leaf"));
            app.activate(0, window, cx);
            assert!(
                app.maximized.is_none(),
                "a stashed zoom whose pane is gone stays gone"
            );
        });
    }
}

// A test window has no daemon behind it — its socket path is under the pinned
// test config dir and nothing is listening on it — so every forward request
// fails. That is exactly the case these are about: what the panel and the form
// are left holding when the far side does not answer.
#[cfg(all(test, unix))]
mod managed_forward_gpui_tests {
    use gpui::TestAppContext;
    use gpui_component::input::InputState;

    use crate::daemon::protocol::{ForwardStatus, ManagedForward, SshForwardKind};
    use crate::ui::app::test_window::harness_with_tabs;

    fn listening(id: u64) -> ManagedForward {
        ManagedForward {
            id,
            pane_id: 1,
            kind: SshForwardKind::Local,
            bind_host: "127.0.0.1".to_string(),
            bind_port: 8080,
            target_host: "10.0.0.5".to_string(),
            target_port: 80,
            description: None,
            status: ForwardStatus::Listening,
        }
    }

    #[gpui::test]
    fn an_add_that_never_reaches_the_session_leaves_the_panel_as_it_was(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        app.update_in(&mut vcx, |app, window, cx| {
            app.loopback_panel.managed = vec![listening(1)];
            app.loopback_panel.form_pane_id = Some(1);
            let typed: [(&gpui::Entity<InputState>, &str); 3] = [
                (&app.loopback_panel.mf_bind_port, "9000"),
                (&app.loopback_panel.mf_target_host, "127.0.0.1"),
                (&app.loopback_panel.mf_target_port, "22"),
            ];
            for (input, value) in typed {
                input.update(cx, |input, cx| input.set_value(value, window, cx));
            }

            app.add_managed_forward(1, window, cx);

            assert_eq!(
                app.loopback_panel.managed.len(),
                1,
                "a request that failed says nothing about the forwards that are up"
            );
            assert!(
                app.loopback_panel.mf_error.is_some(),
                "and the form has to say why the Add did nothing"
            );
            assert_eq!(
                app.loopback_panel.form_pane_id,
                Some(1),
                "the form stays open on what was typed"
            );
        });
    }

    #[gpui::test]
    fn a_save_that_cannot_be_made_leaves_the_rule_it_would_replace_alone(cx: &mut TestAppContext) {
        let (app, mut vcx, _streams) = harness_with_tabs(cx, 1);

        app.update_in(&mut vcx, |app, window, cx| {
            app.loopback_panel.managed = vec![listening(1)];
            app.loopback_panel.form_pane_id = Some(1);
            app.loopback_panel.mf_editing = Some(listening(1));
            let typed: [(&gpui::Entity<InputState>, &str); 3] = [
                (&app.loopback_panel.mf_bind_port, "8080"),
                (&app.loopback_panel.mf_target_host, "10.0.0.6"),
                (&app.loopback_panel.mf_target_port, "80"),
            ];
            for (input, value) in typed {
                input.update(cx, |input, cx| input.set_value(value, window, cx));
            }

            app.add_managed_forward(1, window, cx);

            assert_eq!(
                app.loopback_panel.managed,
                vec![listening(1)],
                "the rule being edited must survive an edit that could not be made"
            );
            assert!(
                app.loopback_panel.mf_editing.is_some(),
                "the form is still editing it"
            );
            assert!(app.loopback_panel.mf_error.is_some());
        });
    }
}

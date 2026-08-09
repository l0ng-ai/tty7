use gpui::{
    App, Background, Hsla, Menu, MenuItem, OsAction, Pixels, Point, SystemMenuType, Window,
    WindowBackgroundAppearance, linear_color_stop, linear_gradient, point, px, rgb,
};
use gpui_component::scroll::ScrollbarShow;
use gpui_component::{ActiveTheme, Theme, ThemeMode};

use crate::core::actions::*;
use crate::core::config::{Config, WindowBackdrop};
use crate::terminal::view::{
    ClearScrollback, CopyText, CutText, FindInTerminal, FindNext, FindPrevious, PasteText,
    RedoEdit, SelectAll, UndoEdit,
};
use crate::ui::i18n::{L10nKey, t};
use crate::ui::presets;
use crate::ui::presets::Fill;
#[cfg(target_os = "windows")]
use std::sync::OnceLock;

pub(crate) fn traffic_light_position() -> Point<Pixels> {
    point(px(9.), px(13.))
}

pub(crate) fn set_menus(cx: &mut App) {
    cx.set_menus([
        Menu::new("tty7").items([
            MenuItem::action(t(L10nKey::AppMenuAbout), About),
            MenuItem::action(t(L10nKey::AppMenuCheckForUpdates), CheckForUpdates),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuSettings), OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu(t(L10nKey::AppMenuServices), SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuHideApp), HideApp),
            MenuItem::action(t(L10nKey::AppMenuHideOthers), HideOthers),
            MenuItem::action(t(L10nKey::AppMenuShowAll), ShowAll),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuQuit), Quit),
        ]),
        Menu::new(t(L10nKey::AppMenuFile)).items([
            MenuItem::action(t(L10nKey::AppMenuNewTab), NewTab),
            MenuItem::action(t(L10nKey::AppMenuNewWorkspace), NewWorkspace),
            MenuItem::action(t(L10nKey::AppMenuNewWorktreeTab), NewWorktreeTab),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuSplitRight), SplitRight),
            MenuItem::action(t(L10nKey::AppMenuSplitDown), SplitDown),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuRenameTab), RenameTab),
            MenuItem::action(
                t(L10nKey::AppMenuCopyWorkingDirectory),
                CopyWorkingDirectory,
            ),
            MenuItem::action(t(L10nKey::AppMenuCopySessionId), CopyAgentSessionId),
            MenuItem::action(t(L10nKey::AppMenuForkSession), ForkAgentSession),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuClosePaneTab), CloseActiveTab),
            MenuItem::action(t(L10nKey::AppMenuCloseOtherTabs), CloseOtherTabs),
            MenuItem::action(t(L10nKey::AppMenuCloseTabsRight), CloseTabsToTheRight),
            MenuItem::action(t(L10nKey::AppMenuReopenClosedTab), ReopenClosedTab),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuRenameWorkspace), RenameWorkspace),
            MenuItem::action(t(L10nKey::AppMenuStopWorkspace), StopWorkspace),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuDeleteWorkspace), DeleteWorkspace),
        ]),
        Menu::new(t(L10nKey::AppMenuEdit)).items([
            MenuItem::os_action(t(L10nKey::AppMenuUndo), UndoEdit, OsAction::Undo),
            MenuItem::os_action(t(L10nKey::AppMenuRedo), RedoEdit, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action(t(L10nKey::AppMenuCut), CutText, OsAction::Cut),
            MenuItem::os_action(t(L10nKey::AppMenuCopy), CopyText, OsAction::Copy),
            MenuItem::os_action(t(L10nKey::AppMenuPaste), PasteText, OsAction::Paste),
            MenuItem::os_action(t(L10nKey::AppMenuSelectAll), SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuFind), FindInTerminal),
            MenuItem::action(t(L10nKey::AppMenuFindNext), FindNext),
            MenuItem::action(t(L10nKey::AppMenuFindPrevious), FindPrevious),
        ]),
        Menu::new(t(L10nKey::AppMenuView)).items([
            MenuItem::action(t(L10nKey::AppMenuCommandPalette), TogglePalette),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuIncreaseFontSize), IncreaseFontSize),
            MenuItem::action(t(L10nKey::AppMenuDecreaseFontSize), DecreaseFontSize),
            MenuItem::action(t(L10nKey::AppMenuResetFontSize), ResetFontSize),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuLeftSidebar), ToggleLeftPanel),
            MenuItem::action(t(L10nKey::AppMenuRightPanel), ToggleRightPanel),
            MenuItem::action(t(L10nKey::AppMenuCodePanel), ToggleCodePanel),
            MenuItem::action(t(L10nKey::AppMenuTabBarPosition), ToggleTabSidebar),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuFocusNextPane), FocusNextPane),
            MenuItem::action(t(L10nKey::AppMenuFocusPreviousPane), FocusPrevPane),
            MenuItem::action(t(L10nKey::AppMenuZoomPane), ToggleMaximizePane),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuClearScrollback), ClearScrollback),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuEnterFullscreen), ToggleFullscreen),
        ]),
        Menu::new(t(L10nKey::AppMenuWindow)).items(window_menu_items(cx)),
        Menu::new(t(L10nKey::AppMenuHelp)).items([
            MenuItem::action(t(L10nKey::AppMenuDocumentation), OpenDocumentation),
            MenuItem::action(t(L10nKey::AppMenuKeyboardShortcuts), ShowKeyboardShortcuts),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuJoinDiscord), OpenDiscord),
            MenuItem::action(t(L10nKey::AppMenuReportIssue), ReportIssue),
            MenuItem::separator(),
            MenuItem::action(t(L10nKey::AppMenuRestartServer), RestartDaemon),
        ]),
    ]);
}

fn window_menu_items(cx: &App) -> Vec<MenuItem> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let order = crate::ui::windows::menu_order(cx);
    let store = crate::core::session::WorkspaceStore::all(cx);

    let slot_action = crate::ui::tab_strip::select_workspace_action;

    let mut items = vec![
        MenuItem::action(t(L10nKey::AppMenuMinimize), MinimizeWindow),
        MenuItem::action(t(L10nKey::AppMenuZoom), ZoomWindow),
        MenuItem::separator(),
    ];
    let workspace_start = items.len();
    let mut separated = false;
    for (i, (id, open)) in order.iter().enumerate() {
        let Some(workspace) = store.get(*id) else {
            continue;
        };
        let Some(action) = slot_action(i) else { break };
        if !open && !separated {
            separated = true;
            if items.len() > workspace_start {
                items.push(MenuItem::Separator);
            }
        }
        let name = crate::ui::machine_mirror::display_name(cx, workspace)
            .unwrap_or_else(|| t(L10nKey::WindowUntitled).to_string());
        let label = if *open {
            name
        } else {
            format!(
                "{}  —  {}",
                name,
                crate::ui::home::relative_time(now, workspace.last_active)
            )
        };
        items.push(MenuItem::Action {
            name: label.into(),
            action,
            os_action: None,
            checked: false,
            disabled: false,
        });
    }
    if items.len() == workspace_start {
        items.push(MenuItem::action(
            t(L10nKey::AppMenuNewWorkspace),
            NewWorkspace,
        ));
    }
    items
}

pub(crate) fn window_background(bg: &presets::ActiveBackground) -> Background {
    window_background_with_alpha(bg, bg.opacity.unwrap_or(1.0))
}

/// The same preset fill with the alpha channel forced to 1 — used by the
/// settings overlay, which must stay opaque (workspace translucency must
/// never show through it) while still rendering the preset's gradient
/// design instead of collapsing to a flat solid color.
pub(crate) fn window_background_opaque(bg: &presets::ActiveBackground) -> Background {
    window_background_with_alpha(bg, 1.0)
}

/// The workspace's own fill: the preset at whatever alpha the window opacity
/// and backdrop material asked for.
pub(crate) fn workspace_background(cx: &App) -> Background {
    match cx.try_global::<presets::ActiveBackground>() {
        Some(bg) => window_background(bg),
        None => cx.theme().background.into(),
    }
}

/// The fill for a full-window overlay (settings, the opened file, the diff
/// view). Always opaque: the overlay covers the whole workspace, so window
/// translucency and the backdrop material must stop at it instead of showing
/// desktop through its text. Overlays pair this with
/// `app::overlay_surface_layers`, because their own opaque fill hides the
/// theme background image the workspace root paints beneath them.
pub(crate) fn overlay_background(cx: &App) -> Background {
    match cx.try_global::<presets::ActiveBackground>() {
        Some(bg) => window_background_opaque(bg),
        None => cx.theme().background.alpha(1.0).into(),
    }
}

fn window_background_with_alpha(bg: &presets::ActiveBackground, alpha: f32) -> Background {
    let stop = |c: u32| -> Hsla {
        let mut h: Hsla = rgb(c).into();
        h.a = alpha;
        h
    };
    match bg.fill {
        Fill::Solid(c) => stop(c).into(),
        Fill::Vertical { top, bottom } => linear_gradient(
            180.,
            linear_color_stop(stop(top), 0.),
            linear_color_stop(stop(bottom), 1.),
        ),
        Fill::Horizontal { left, right } => linear_gradient(
            90.,
            linear_color_stop(stop(left), 0.),
            linear_color_stop(stop(right), 1.),
        ),
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SystemAppearance {
    dark: bool,
}

impl gpui::Global for SystemAppearance {}

fn is_dark(appearance: gpui::WindowAppearance) -> bool {
    matches!(
        appearance,
        gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
    )
}

pub(crate) fn refresh_system_appearance(cx: &mut App) {
    let dark = is_dark(cx.window_appearance());
    cx.set_global(SystemAppearance { dark });
}

pub(crate) fn note_system_appearance(window: &Window, cx: &mut App) {
    let dark = is_dark(window.appearance());
    cx.set_global(SystemAppearance { dark });
}

pub(crate) fn system_dark(cx: &App) -> bool {
    cx.try_global::<SystemAppearance>()
        .is_some_and(|appearance| appearance.dark)
}

pub(crate) fn effective_preset_id(cx: &App) -> String {
    let config = cx.global::<Config>();
    if !config.theme_follow_system {
        config.theme_preset.clone()
    } else if system_dark(cx) {
        config.theme_preset_dark.clone()
    } else {
        config.theme_preset_light.clone()
    }
}

/// Default background alpha used while a backdrop material is active and
/// neither the theme nor the config overrides the opacity — a fully opaque
/// fill would hide the material behind it.
pub(crate) const SYSTEM_MATERIAL_OPACITY: f32 = 0.82;

/// Maps the requested backdrop to the gpui window appearance, falling back
/// down the chain when the OS build predates a material: Mica needs Windows
/// 11 22H2 (build 22621), acrylic needs 1809 (build 17763); below that plain
/// translucency is the best Windows can do. Pure function so the fallback
/// table is unit-testable on every platform.
pub(crate) fn windows_background_appearance(
    backdrop: WindowBackdrop,
    blur: bool,
    build: u32,
) -> WindowBackgroundAppearance {
    match backdrop {
        WindowBackdrop::Auto => {
            if blur {
                WindowBackgroundAppearance::Blurred
            } else {
                WindowBackgroundAppearance::Transparent
            }
        }
        WindowBackdrop::Blur => {
            if build >= 17_763 {
                WindowBackgroundAppearance::Blurred
            } else {
                WindowBackgroundAppearance::Transparent
            }
        }
        WindowBackdrop::Mica => {
            if build >= 22_621 {
                WindowBackgroundAppearance::MicaBackdrop
            } else if build >= 17_763 {
                WindowBackgroundAppearance::Blurred
            } else {
                WindowBackgroundAppearance::Transparent
            }
        }
        WindowBackdrop::MicaAlt => {
            if build >= 22_621 {
                WindowBackgroundAppearance::MicaAltBackdrop
            } else if build >= 17_763 {
                WindowBackgroundAppearance::Blurred
            } else {
                WindowBackgroundAppearance::Transparent
            }
        }
        WindowBackdrop::Acrylic => {
            if build >= 22_621 {
                // Native acrylic (DWMSBT_TRANSIENTWINDOW) on Windows 11 22H2+.
                WindowBackgroundAppearance::AcrylicBackdrop
            } else if build >= 17_763 {
                // Classic WCA acrylic (ACCENT_ENABLE_ACRYLICBLURBEHIND).
                WindowBackgroundAppearance::Blurred
            } else {
                WindowBackgroundAppearance::Transparent
            }
        }
        WindowBackdrop::Off => WindowBackgroundAppearance::Transparent,
    }
}

/// The backdrop presets actually worth offering on a given Windows build.
/// Mica, Mica Alt and native acrylic all need Windows 11 22H2 (22621); below
/// that every one of them collapses onto the same classic WCA blur that
/// `Blur` already offers, so listing them would present several choices that
/// render identically — hiding them beats misrepresenting them. Below 1809
/// (17763) there is no blur API at all, so only plain translucency is left.
/// Pure function so the table is unit-testable on every platform.
pub(crate) fn supported_backdrops_for(build: u32) -> &'static [WindowBackdrop] {
    if build >= 22_621 {
        &[
            WindowBackdrop::Auto,
            WindowBackdrop::Blur,
            WindowBackdrop::Mica,
            WindowBackdrop::MicaAlt,
            WindowBackdrop::Acrylic,
            WindowBackdrop::Off,
        ]
    } else if build >= 17_763 {
        &[
            WindowBackdrop::Auto,
            WindowBackdrop::Blur,
            WindowBackdrop::Off,
        ]
    } else {
        &[WindowBackdrop::Auto, WindowBackdrop::Off]
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn supported_backdrops() -> &'static [WindowBackdrop] {
    supported_backdrops_for(windows_build_number())
}

#[cfg(target_os = "windows")]
pub(crate) fn windows_build_number() -> u32 {
    static BUILD: OnceLock<u32> = OnceLock::new();
    *BUILD.get_or_init(|| {
        use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
        use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
        let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
        // RtlGetVersion is not subject to the app-compat version lying that
        // GetVersionEx is, and unlike GetVersionEx it does not require
        // dwOSVersionInfoSize to be pre-filled; set it anyway so the call
        // matches the documented contract.
        info.dwOSVersionInfoSize = std::mem::size_of_val(&info) as u32;
        let status = unsafe { RtlGetVersion(&mut info) };
        if status == 0 {
            info.dwBuildNumber
        } else {
            // Unknown build: treat it as too old, so the fallback chain picks
            // the most conservative material instead of a silent no-op.
            0
        }
    })
}

pub(crate) fn resolved_background_appearance(
    backdrop: WindowBackdrop,
    blur: bool,
) -> WindowBackgroundAppearance {
    #[cfg(target_os = "windows")]
    {
        windows_background_appearance(backdrop, blur, windows_build_number())
    }
    #[cfg(not(target_os = "windows"))]
    {
        // `window_backdrop` is a Windows-only setting and this platform has
        // no UI to change or clear it (only "Reset window overrides", which
        // also clobbers opacity). If any variant pinned the appearance
        // here, the local blur switch would silently show a checked state
        // that isn't rendered — so every variant, including Off and Blur,
        // defers to the legacy toggle. (A Windows-synced "off" therefore
        // does not force transparency on macOS/Linux; the local blur switch
        // stays authoritative.)
        if blur {
            WindowBackgroundAppearance::Blurred
        } else {
            WindowBackgroundAppearance::Transparent
        }
    }
}

/// Whether the resolved backdrop on this platform is actually a material —
/// i.e. the window content sits over a blur/mica layer instead of plain
/// transparency. Derived from `resolved_background_appearance` so the
/// opacity defaulting and the appearance resolution can never disagree: on
/// very old builds `Blur`/`Acrylic` fall back to `Transparent`, and a
/// see-through window with no material behind it gets no special opacity.
///
/// `Auto` never counts, even when the legacy blur toggle resolves it to
/// `Blurred`. It is the default for every config written before this
/// setting existed, and those windows were opaque with opaque sidebars —
/// treating them as materials would silently turn an untouched install
/// see-through on first launch after an update. Only an explicit pick in
/// the backdrop dropdown opts a window into the translucent defaults.
pub(crate) fn material_active(backdrop: WindowBackdrop, blur: bool) -> bool {
    cfg!(target_os = "windows")
        && backdrop != WindowBackdrop::Auto
        && matches!(
            resolved_background_appearance(backdrop, blur),
            WindowBackgroundAppearance::Blurred
                | WindowBackgroundAppearance::MicaBackdrop
                | WindowBackgroundAppearance::MicaAltBackdrop
                | WindowBackgroundAppearance::AcrylicBackdrop
        )
}

/// The window's background alpha when neither the config nor the theme sets
/// an explicit opacity: translucent enough to show an active material,
/// fully opaque otherwise. Single source of truth for `apply_theme` and the
/// settings slider, so the two can never drift apart.
pub(crate) fn default_window_opacity(backdrop: WindowBackdrop, blur: bool) -> f32 {
    if material_active(backdrop, blur) {
        SYSTEM_MATERIAL_OPACITY
    } else {
        1.0
    }
}

/// The fill for the workspace's large translucent surfaces (file sidebar,
/// right panel, SFTP panel). While a material is active and the window is
/// translucent, the surface paints on top of the already-alpha window
/// background, so its own alpha stacks (src-over) and the material would
/// show through far less than behind the terminal; a constant 0.15 keeps
/// the backdrop ratio at ~85% of the terminal's at every opacity setting.
/// `theme.sidebar` itself stays opaque — the settings theme picker paints
/// with it on top of the opaque settings overlay and must stay legible.
pub(crate) fn workspace_surface_color(cx: &App) -> Hsla {
    let base: Hsla = cx.theme().sidebar;
    let translucent = cx
        .try_global::<presets::ActiveBackground>()
        .and_then(|bg| bg.opacity)
        .is_some_and(|o| o < 1.0);
    if !translucent {
        return base;
    }
    let config = cx.global::<Config>();
    let theme = presets::by_id(cx, &effective_preset_id(cx));
    let blur = config.window_blur.unwrap_or(theme.blur);
    if material_active(config.window_backdrop, blur) {
        base.alpha(0.15)
    } else {
        base
    }
}

/// The backdrop presets offered in the settings dropdown: everything the
/// current build supports, plus the stored value even if it is not
/// supported here (e.g. Mica synced from a newer machine) — so the label
/// always matches what the window actually resolves to instead of quietly
/// showing "Auto" while a fallback appearance is applied.
#[cfg(target_os = "windows")]
pub(crate) fn backdrop_options(current: WindowBackdrop) -> Vec<WindowBackdrop> {
    let mut list = supported_backdrops().to_vec();
    if !list.contains(&current) {
        list.push(current);
    }
    list
}

pub(crate) fn background_appearance(cx: &App) -> WindowBackgroundAppearance {
    let config = cx.global::<Config>();
    let theme = presets::by_id(cx, &effective_preset_id(cx));
    let blur = config.window_blur.unwrap_or(theme.blur);
    resolved_background_appearance(config.window_backdrop, blur)
}

/// The appearance last handed to each live window, so `apply_theme` can skip
/// a `set_background_appearance` that would change nothing.
///
/// Worth the bookkeeping because the call is no longer cheap: with a DWM
/// material selected, gpui answers it by re-setting
/// `DWMWA_SYSTEMBACKDROP_TYPE` and forcing a non-client frame recalculation
/// (`SetWindowPos` with `SWP_FRAMECHANGED`). `apply_theme` runs on *every*
/// `Config` mutation in *every* window, so dragging the opacity slider —
/// which never changes the appearance — would otherwise recalc the frame
/// once per mouse-move sample and visibly stutter the drag.
#[derive(Default)]
struct AppliedAppearance(std::collections::HashMap<gpui::WindowId, WindowBackgroundAppearance>);

impl gpui::Global for AppliedAppearance {}

/// Records `appearance` for `window` and reports whether it differs from
/// what that window was last given. Entries for closed windows are dropped
/// first: window ids come from a slotmap and are reused, so a stale entry
/// could otherwise suppress the very first call for a brand-new window.
fn take_appearance_change(
    window: &Window,
    appearance: WindowBackgroundAppearance,
    cx: &mut App,
) -> bool {
    let id = window.window_handle().window_id();
    let live: std::collections::HashSet<gpui::WindowId> =
        cx.windows().iter().map(|w| w.window_id()).collect();
    let applied = cx.default_global::<AppliedAppearance>();
    applied.0.retain(|known, _| live.contains(known));
    applied.0.insert(id, appearance) != Some(appearance)
}

pub(crate) fn apply_theme(mut window: Option<&mut Window>, cx: &mut App) {
    let follow = cx.global::<Config>().theme_follow_system;
    if follow {
        sync_native_appearance(None);
        #[cfg(target_os = "macos")]
        refresh_system_appearance(cx);
    }
    let theme = presets::by_id(cx, &effective_preset_id(cx));
    // Cache the mode beside the machine tree: the daemon is a separate process
    // and reads it back when it spawns a Windows pane, where ConPTY drops an
    // OSC 11 background query before tty7's emulator can answer it. Derived
    // state, so it deliberately stays out of the user's `config.json`.
    crate::core::machine::note_appearance(theme.dark);
    let config = cx.global::<Config>();
    let mode = if theme.dark {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    let blur = config.window_blur.unwrap_or(theme.blur);
    // A material needs some translucency to be visible; without an explicit
    // opacity override it defaults to SYSTEM_MATERIAL_OPACITY instead of
    // 1.0. Derived from the *resolved* appearance so old builds where
    // Blur/Acrylic fall back to plain transparency stay opaque by default.
    let default_opacity = default_window_opacity(config.window_backdrop, blur);
    let opacity = config
        .window_opacity
        .or(theme.opacity)
        .unwrap_or(default_opacity);
    let opacity = (opacity < 1.0).then_some(opacity);
    if !follow {
        sync_native_appearance(Some(theme.dark));
    }
    let m = theme.neutrals();
    let surfaces = theme.surfaces();
    let sem = theme.semantics();
    let active = theme.active_palette(config.theme_legible_palette);
    let auto_hide_scrollbars = cx.should_auto_hide_scrollbars();

    let backdrop = config.window_backdrop;

    if let Some(window) = window.as_deref_mut() {
        let appearance = resolved_background_appearance(backdrop, blur);
        if take_appearance_change(window, appearance, cx) {
            window.set_background_appearance(appearance);
        }
    }

    Theme::change(mode, window.as_deref_mut(), cx);
    cx.set_global(active);
    cx.set_global(presets::ActiveBackground {
        fill: theme.background.clone(),
        opacity,
        image: theme.image.clone(),
    });
    cx.set_global(surfaces.clone());
    cx.set_global(presets::ActiveAccent(m.accent));

    let t = Theme::global_mut(cx);
    let mut base: Hsla = rgb(m.background).into();
    if let Some(o) = opacity {
        base.a = o;
    }
    t.background = base;
    t.foreground = rgb(m.foreground).into();
    t.border = rgb(m.border).into();
    t.secondary = rgb(m.secondary).into();
    t.muted = rgb(m.muted).into();
    t.muted_foreground = rgb(m.muted_foreground).into();
    t.popover = rgb(m.popover).into();
    t.tokens.popover = Hsla::from(rgb(m.popover)).into();
    t.tokens.popover_foreground = Hsla::from(rgb(m.foreground)).into();

    let accent_fill = rgb(surfaces.popover.cursor);
    let accent_text: Hsla = rgb(m.foreground).into();
    t.accent = accent_fill.into();
    t.accent_foreground = accent_text;
    t.tokens.accent = Hsla::from(accent_fill).into();
    t.tokens.accent_foreground = accent_text.into();

    let primary_base: Hsla = rgb(presets::mix(m.foreground, m.background, 0.20)).into();
    let primary_hover: Hsla = rgb(presets::mix(m.foreground, m.background, 0.30)).into();
    let primary_active: Hsla = rgb(presets::mix(m.foreground, m.background, 0.10)).into();
    t.primary = primary_base;
    t.primary_hover = primary_hover;
    t.primary_active = primary_active;
    t.tokens.primary = primary_base.into();
    t.tokens.primary_hover = primary_hover.into();
    t.tokens.primary_active = primary_active.into();
    t.tokens.button_primary = primary_base.into();
    t.tokens.button_primary_hover = primary_hover.into();
    t.tokens.button_primary_active = primary_active.into();

    let steps = |c: u32| {
        (
            Hsla::from(rgb(c)),
            Hsla::from(rgb(presets::mix(c, m.background, 0.15))),
            Hsla::from(rgb(presets::mix(c, m.foreground, 0.15))),
        )
    };

    let (ink, ink_hover, ink_active) = steps(sem.danger.ink);
    let (fill, fill_hover, fill_active) = steps(sem.danger.fill);
    let on_fill = Hsla::from(rgb(sem.danger.on_fill));
    t.danger = ink;
    t.danger_hover = ink_hover;
    t.danger_active = ink_active;
    t.danger_foreground = on_fill;
    t.tokens.danger = fill.into();
    t.tokens.danger_hover = fill_hover.into();
    t.tokens.danger_active = fill_active.into();
    t.tokens.danger_foreground = on_fill.into();
    t.tokens.button_danger = fill.into();
    t.tokens.button_danger_hover = fill_hover.into();
    t.tokens.button_danger_active = fill_active.into();
    t.tokens.button_danger_foreground = on_fill.into();

    let (ink, ink_hover, ink_active) = steps(sem.warning.ink);
    let (fill, fill_hover, fill_active) = steps(sem.warning.fill);
    let on_fill = Hsla::from(rgb(sem.warning.on_fill));
    t.warning = ink;
    t.warning_hover = ink_hover;
    t.warning_active = ink_active;
    t.warning_foreground = on_fill;
    t.tokens.warning = fill.into();
    t.tokens.warning_hover = fill_hover.into();
    t.tokens.warning_active = fill_active.into();
    t.tokens.warning_foreground = on_fill.into();
    t.tokens.button_warning = fill.into();
    t.tokens.button_warning_hover = fill_hover.into();
    t.tokens.button_warning_active = fill_active.into();
    t.tokens.button_warning_foreground = on_fill.into();

    let (ink, ink_hover, ink_active) = steps(sem.success.ink);
    let (fill, fill_hover, fill_active) = steps(sem.success.fill);
    let on_fill = Hsla::from(rgb(sem.success.on_fill));
    t.success = ink;
    t.success_hover = ink_hover;
    t.success_active = ink_active;
    t.success_foreground = on_fill;
    t.tokens.success = fill.into();
    t.tokens.success_hover = fill_hover.into();
    t.tokens.success_active = fill_active.into();
    t.tokens.success_foreground = on_fill.into();
    t.tokens.button_success = fill.into();
    t.tokens.button_success_hover = fill_hover.into();
    t.tokens.button_success_active = fill_active.into();
    t.tokens.button_success_foreground = on_fill.into();

    let (ink, ink_hover, ink_active) = steps(sem.info.ink);
    let (fill, fill_hover, fill_active) = steps(sem.info.fill);
    let on_fill = Hsla::from(rgb(sem.info.on_fill));
    t.info = ink;
    t.info_hover = ink_hover;
    t.info_active = ink_active;
    t.info_foreground = on_fill;
    t.tokens.info = fill.into();
    t.tokens.info_hover = fill_hover.into();
    t.tokens.info_active = fill_active.into();
    t.tokens.info_foreground = on_fill.into();
    t.tokens.button_info = fill.into();
    t.tokens.button_info_hover = fill_hover.into();
    t.tokens.button_info_active = fill_active.into();
    t.tokens.button_info_foreground = on_fill.into();

    t.link = rgb(sem.link.ink).into();
    t.link_hover = rgb(presets::mix(sem.link.ink, m.foreground, 0.25)).into();
    t.link_active = rgb(presets::mix(sem.link.ink, m.background, 0.20)).into();
    t.tokens.link = Hsla::from(rgb(sem.link.ink)).into();
    t.tokens.link_hover = Hsla::from(rgb(presets::mix(sem.link.ink, m.foreground, 0.25))).into();
    t.tokens.link_active = Hsla::from(rgb(presets::mix(sem.link.ink, m.background, 0.20))).into();

    let knob = if presets::is_lighter(m.background, m.foreground) {
        m.background
    } else {
        m.foreground
    };
    t.tokens.background = Hsla::from(rgb(m.background)).into();
    t.tokens.switch_thumb = Hsla::from(rgb(knob)).into();
    t.tokens.switch = Hsla::from(rgb(surfaces.window.selected)).into();

    t.caret = rgb(m.caret).into();
    t.selection = rgb(m.selection).into();

    let scrollbar_thumb: Hsla = rgb(presets::mix(m.background, m.foreground, 0.18)).into();
    let scrollbar_thumb_hover: Hsla = rgb(presets::mix(m.background, m.foreground, 0.34)).into();
    t.scrollbar = gpui::transparent_black();
    t.scrollbar_thumb = scrollbar_thumb;
    t.scrollbar_thumb_hover = scrollbar_thumb_hover;
    t.tokens.scrollbar = gpui::transparent_black().into();
    t.tokens.scrollbar_thumb = scrollbar_thumb.into();
    t.tokens.scrollbar_thumb_hover = scrollbar_thumb_hover.into();

    t.scrollbar_show = if auto_hide_scrollbars {
        ScrollbarShow::Scrolling
    } else {
        ScrollbarShow::Always
    };

    t.radius = px(8.);

    let sidebar_bg = Hsla::from(rgb(m.sidebar));
    let sidebar_sel = rgb(surfaces.sidebar.selected);
    // `t.sidebar` stays the opaque theme token: the settings theme picker
    // paints with it on top of the (opaque) settings overlay, so diluting
    // it would wash out that panel. The workspace sidebar/right-panel
    // surfaces get their translucent variant at render time instead —
    // see `workspace_surface_color`.
    t.sidebar = sidebar_bg.into();
    t.tokens.sidebar = sidebar_bg.into();
    t.sidebar_border = rgb(m.border).into();
    t.sidebar_foreground = rgb(surfaces.sidebar.text_resting).into();
    t.sidebar_accent = sidebar_sel.into();
    t.tokens.sidebar_accent = Hsla::from(sidebar_sel).into();
    t.sidebar_accent_foreground = rgb(surfaces.sidebar.text_selected).into();

    t.list.active_highlight = true;
    t.list_active = rgb(surfaces.popover.cursor).into();
    t.list_active_border = rgb(surfaces.popover.cursor).into();
    t.list_hover = rgb(surfaces.popover.hover).into();

    t.input = rgb(surfaces.window.selected).into();
    t.tokens.input = Hsla::from(rgb(surfaces.window.selected)).into();

    let button_hover: Hsla = rgb(surfaces.window.hover).into();
    let button_active: Hsla = rgb(surfaces.window.selected).into();
    t.tokens.button_hover = button_hover.into();
    t.tokens.button_active = button_active.into();
    t.tokens.secondary_hover = button_hover.into();
    t.tokens.secondary_active = button_active.into();
    t.tokens.button_secondary_hover = button_hover.into();
    t.tokens.button_secondary_active = button_active.into();

    t.ring = rgb(m.accent).into();

    #[cfg(target_os = "macos")]
    if let Some(window) = window.as_deref_mut() {
        window.set_traffic_light_position(traffic_light_position());
    }
}

pub(crate) fn switch(id: impl Into<gpui::ElementId>, cx: &App) -> gpui_component::switch::Switch {
    let accent = cx.global::<presets::ActiveAccent>().0;
    gpui_component::switch::Switch::new(id).color(Hsla::from(rgb(accent)))
}

pub(crate) fn apply_cursor_hide_mode(cx: &mut App) {
    let mode = if cx.global::<Config>().mouse_hide_while_typing {
        gpui::CursorHideMode::OnTypingAndAction
    } else {
        gpui::CursorHideMode::Never
    };
    cx.set_cursor_hide_mode(mode);
}

#[cfg(target_os = "macos")]
fn sync_native_appearance(dark: Option<bool>) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    };

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let appearance = dark.and_then(|dark| {
        let name = unsafe {
            if dark {
                NSAppearanceNameDarkAqua
            } else {
                NSAppearanceNameAqua
            }
        };
        NSAppearance::appearanceNamed(name)
    });
    NSApplication::sharedApplication(mtm).setAppearance(appearance.as_deref());
}

#[cfg(not(target_os = "macos"))]
fn sync_native_appearance(_dark: Option<bool>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn effective_preset_follows_the_cached_system_appearance(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(Config(crate::core::config::CoreConfig {
                theme_follow_system: true,
                theme_preset_light: "light-slot".into(),
                theme_preset_dark: "dark-slot".into(),
                ..Default::default()
            }));

            cx.set_global(SystemAppearance { dark: false });
            assert!(!system_dark(cx));
            assert_eq!(effective_preset_id(cx), "light-slot");

            cx.set_global(SystemAppearance { dark: true });
            assert!(system_dark(cx));
            assert_eq!(effective_preset_id(cx), "dark-slot");

            cx.global_mut::<Config>().theme_follow_system = false;
            assert_eq!(effective_preset_id(cx), Config::default().theme_preset);
        });
    }

    #[test]
    fn windows_background_appearance_falls_back_by_build() {
        use crate::core::config::WindowBackdrop;
        let a = |b: WindowBackdrop, blur: bool, build: u32| {
            windows_background_appearance(b, blur, build)
        };

        // Auto keeps the legacy blur/translucent choice.
        assert_eq!(
            a(WindowBackdrop::Auto, false, 22_621),
            WindowBackgroundAppearance::Transparent
        );
        assert_eq!(
            a(WindowBackdrop::Auto, true, 17_763),
            WindowBackgroundAppearance::Blurred
        );

        // Blur explicitly requests the classic blurred appearance from 1809
        // on; older builds get plain translucency.
        assert_eq!(
            a(WindowBackdrop::Blur, false, 17_763),
            WindowBackgroundAppearance::Blurred
        );
        assert_eq!(
            a(WindowBackdrop::Blur, true, 17_762),
            WindowBackgroundAppearance::Transparent
        );

        // Mica needs Windows 11 22H2; 1809..22H2 falls back to classic
        // acrylic, and below 1809 there is no blur API to fall back to —
        // asking for one there would leave an unbacked see-through window.
        assert_eq!(
            a(WindowBackdrop::Mica, false, 22_621),
            WindowBackgroundAppearance::MicaBackdrop
        );
        assert_eq!(
            a(WindowBackdrop::Mica, false, 22_620),
            WindowBackgroundAppearance::Blurred
        );
        assert_eq!(
            a(WindowBackdrop::Mica, false, 17_762),
            WindowBackgroundAppearance::Transparent
        );
        assert_eq!(
            a(WindowBackdrop::MicaAlt, false, 26_000),
            WindowBackgroundAppearance::MicaAltBackdrop
        );
        assert_eq!(
            a(WindowBackdrop::MicaAlt, false, 17_763),
            WindowBackgroundAppearance::Blurred
        );
        assert_eq!(
            a(WindowBackdrop::MicaAlt, false, 17_762),
            WindowBackgroundAppearance::Transparent
        );
        // A failed RtlGetVersion reports build 0; every material must degrade
        // to plain translucency there rather than request an unsupported one.
        for backdrop in [
            WindowBackdrop::Blur,
            WindowBackdrop::Mica,
            WindowBackdrop::MicaAlt,
            WindowBackdrop::Acrylic,
        ] {
            assert_eq!(
                a(backdrop, false, 0),
                WindowBackgroundAppearance::Transparent,
                "{backdrop:?} on an unknown build must not request a material"
            );
        }

        // Acrylic uses the native DWMSBT_TRANSIENTWINDOW material on 22H2+,
        // classic WCA acrylic from 1809 on, plain translucency below that.
        assert_eq!(
            a(WindowBackdrop::Acrylic, false, 22_621),
            WindowBackgroundAppearance::AcrylicBackdrop
        );
        assert_eq!(
            a(WindowBackdrop::Acrylic, false, 22_620),
            WindowBackgroundAppearance::Blurred
        );
        assert_eq!(
            a(WindowBackdrop::Acrylic, false, 17_763),
            WindowBackgroundAppearance::Blurred
        );
        assert_eq!(
            a(WindowBackdrop::Acrylic, false, 17_762),
            WindowBackgroundAppearance::Transparent
        );

        // Off never requests a material.
        assert_eq!(
            a(WindowBackdrop::Off, false, 26_000),
            WindowBackgroundAppearance::Transparent
        );
        assert_eq!(
            a(WindowBackdrop::Off, true, 26_000),
            WindowBackgroundAppearance::Transparent
        );
    }

    #[test]
    fn auto_never_counts_as_a_material() {
        use crate::core::config::WindowBackdrop;
        // Every config written before this setting existed carries `Auto`,
        // and many of them carry `window_blur: true` from the old switch.
        // Those windows were opaque, with opaque sidebars — an update must
        // not silently turn them see-through, so `Auto` gets the plain 1.0
        // default no matter which way the legacy toggle points.
        for blur in [false, true] {
            assert!(!material_active(WindowBackdrop::Auto, blur));
            assert_eq!(default_window_opacity(WindowBackdrop::Auto, blur), 1.0);
        }
        // `Off` is an explicit "no material" and must behave the same.
        assert!(!material_active(WindowBackdrop::Off, true));
        assert_eq!(default_window_opacity(WindowBackdrop::Off, true), 1.0);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn an_explicitly_picked_material_lowers_the_default_opacity() {
        use crate::core::config::WindowBackdrop;
        // The counterpart to the check above: opting in through the dropdown
        // is what buys the translucent default, on a build that supports it.
        if windows_build_number() < 22_621 {
            return;
        }
        for backdrop in [
            WindowBackdrop::Blur,
            WindowBackdrop::Mica,
            WindowBackdrop::MicaAlt,
            WindowBackdrop::Acrylic,
        ] {
            assert!(material_active(backdrop, false), "{backdrop:?}");
            assert_eq!(
                default_window_opacity(backdrop, false),
                SYSTEM_MATERIAL_OPACITY,
                "{backdrop:?}"
            );
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn non_windows_backdrop_defers_to_the_local_blur_toggle() {
        use crate::core::config::WindowBackdrop;
        // On non-Windows, `window_backdrop` is a Windows-only setting with
        // no local UI to clear it; every variant must defer to the local
        // blur toggle so the switch's checked state always matches what is
        // actually rendered.
        for backdrop in [
            WindowBackdrop::Auto,
            WindowBackdrop::Blur,
            WindowBackdrop::Mica,
            WindowBackdrop::MicaAlt,
            WindowBackdrop::Acrylic,
            WindowBackdrop::Off,
        ] {
            assert_eq!(
                resolved_background_appearance(backdrop, true),
                WindowBackgroundAppearance::Blurred,
                "{backdrop:?} with blur on should resolve to Blurred"
            );
            assert_eq!(
                resolved_background_appearance(backdrop, false),
                WindowBackgroundAppearance::Transparent,
                "{backdrop:?} with blur off should resolve to Transparent"
            );
        }
    }

    #[test]
    fn supported_backdrops_follow_the_build() {
        use crate::core::config::WindowBackdrop;
        assert_eq!(
            supported_backdrops_for(22_621),
            &[
                WindowBackdrop::Auto,
                WindowBackdrop::Blur,
                WindowBackdrop::Mica,
                WindowBackdrop::MicaAlt,
                WindowBackdrop::Acrylic,
                WindowBackdrop::Off,
            ]
        );
        // Pre-22H2: Mica, Mica Alt and Acrylic all collapse onto the very
        // same classic WCA blur that `Blur` already offers, so the dropdown
        // hides them rather than listing choices that render identically.
        assert_eq!(
            supported_backdrops_for(22_620),
            &[
                WindowBackdrop::Auto,
                WindowBackdrop::Blur,
                WindowBackdrop::Off,
            ]
        );
        assert_eq!(
            supported_backdrops_for(17_763),
            &[
                WindowBackdrop::Auto,
                WindowBackdrop::Blur,
                WindowBackdrop::Off,
            ]
        );
        // Every offered preset must resolve to a distinct appearance, or the
        // dropdown is promising a difference the window cannot deliver.
        // `Auto` is exempt: it has no fixed appearance of its own, it mirrors
        // whichever way the blur toggle happens to be set.
        for build in [22_621, 22_620, 17_763, 17_762] {
            let offered: Vec<_> = supported_backdrops_for(build)
                .iter()
                .copied()
                .filter(|b| *b != WindowBackdrop::Auto)
                .collect();
            for (i, a) in offered.iter().enumerate() {
                for b in &offered[i + 1..] {
                    assert_ne!(
                        windows_background_appearance(*a, false, build),
                        windows_background_appearance(*b, false, build),
                        "build {build}: {a:?} and {b:?} render identically"
                    );
                }
            }
        }
        // Below 1809 there is no blur API at all; only plain translucency.
        assert_eq!(
            supported_backdrops_for(17_762),
            &[WindowBackdrop::Auto, WindowBackdrop::Off]
        );
    }
}

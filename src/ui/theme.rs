use gpui::{
    App, Background, Hsla, Menu, MenuItem, OsAction, Pixels, Point, SystemMenuType, Window,
    WindowBackgroundAppearance, linear_color_stop, linear_gradient, point, px, rgb,
};
use gpui_component::scroll::ScrollbarShow;
use gpui_component::{Theme, ThemeMode};

use crate::core::actions::*;
use crate::core::config::Config;
use crate::terminal::view::{
    ClearScrollback, CopyText, CutText, FindInTerminal, FindNext, FindPrevious, PasteText,
    RedoEdit, SelectAll, UndoEdit,
};
use crate::ui::i18n::{L10nKey, t};
use crate::ui::presets;
use crate::ui::presets::Fill;

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
    let alpha = bg.opacity.unwrap_or(1.0);
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

pub(crate) fn background_appearance(cx: &App) -> WindowBackgroundAppearance {
    let config = cx.global::<Config>();
    let theme = presets::by_id(cx, &effective_preset_id(cx));
    if config.window_blur.unwrap_or(theme.blur) {
        WindowBackgroundAppearance::Blurred
    } else {
        WindowBackgroundAppearance::Transparent
    }
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
    let opacity = config.window_opacity.or(theme.opacity).filter(|o| *o < 1.0);
    let blur = config.window_blur.unwrap_or(theme.blur);
    if !follow {
        sync_native_appearance(Some(theme.dark));
    }
    let m = theme.neutrals();
    let surfaces = theme.surfaces();
    let sem = theme.semantics();
    let active = theme.active_palette();
    let auto_hide_scrollbars = cx.should_auto_hide_scrollbars();

    if let Some(window) = window.as_deref_mut() {
        let bg_appearance = if blur {
            WindowBackgroundAppearance::Blurred
        } else {
            WindowBackgroundAppearance::Transparent
        };
        window.set_background_appearance(bg_appearance);
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

    let sidebar_bg = rgb(m.sidebar);
    let sidebar_sel = rgb(surfaces.sidebar.selected);
    t.sidebar = sidebar_bg.into();
    t.tokens.sidebar = Hsla::from(sidebar_bg).into();
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
}

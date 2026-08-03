use gpui::{
    AnyWindowHandle, App, AppContext as _, BorrowAppContext as _, Bounds, Global, Styled as _,
    TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions, point, px, size,
};
use gpui_component::{Root, TitleBar};

use crate::core::config::{Config, StartupMode};
use crate::core::session::{WorkspaceId, WorkspaceStore};
use crate::core::window_state::{WindowGeometry as _, WindowState};
use crate::ui::app::Tty7App;

const CASCADE_STEP: f32 = 28.0;

const DEFAULT_SIZE: (f32, f32) = (1440.0, 900.0);

struct WindowEntry {
    workspace: WorkspaceId,
    handle: AnyWindowHandle,
    app: WeakEntity<Tty7App>,
}

#[derive(Default)]
pub struct WindowRegistry {
    windows: Vec<WindowEntry>,
}

impl Global for WindowRegistry {}

impl WindowRegistry {
    pub fn init(cx: &mut App) {
        cx.set_global(Self::default());
    }

    pub fn count(cx: &mut App) -> usize {
        Self::sweep(cx);
        cx.global::<Self>().windows.len()
    }

    pub fn open_windows(cx: &mut App) -> Vec<(WorkspaceId, WeakEntity<Tty7App>)> {
        Self::sweep(cx);
        cx.global::<Self>()
            .windows
            .iter()
            .map(|w| (w.workspace, w.app.clone()))
            .collect()
    }

    pub fn window_for(cx: &mut App, workspace: WorkspaceId) -> Option<AnyWindowHandle> {
        Self::sweep(cx);
        cx.global::<Self>()
            .windows
            .iter()
            .find(|w| w.workspace == workspace)
            .map(|w| w.handle)
    }

    pub fn most_recent(cx: &mut App) -> Option<WorkspaceId> {
        Self::sweep(cx);
        let active = WorkspaceStore::all(cx).active;
        let registry = cx.global::<Self>();
        active
            .filter(|id| registry.windows.iter().any(|w| w.workspace == *id))
            .or_else(|| registry.windows.first().map(|w| w.workspace))
    }

    pub fn most_recent_local(cx: &mut App) -> Option<WorkspaceId> {
        Self::sweep(cx);
        let views = WorkspaceStore::all(cx);
        let registry = cx.global::<Self>();
        let is_open_local = |id: WorkspaceId| {
            registry.windows.iter().any(|window| window.workspace == id)
                && views.get(id).is_some_and(|view| !view.is_remote())
        };
        views.active.filter(|id| is_open_local(*id)).or_else(|| {
            registry
                .windows
                .iter()
                .filter(|window| is_open_local(window.workspace))
                .max_by_key(|window| {
                    views
                        .get(window.workspace)
                        .map(|view| view.last_active)
                        .unwrap_or_default()
                })
                .map(|window| window.workspace)
        })
    }

    pub fn app_in(cx: &mut App, window: &Window) -> Option<gpui::Entity<Tty7App>> {
        Self::sweep(cx);
        let handle = window.window_handle();
        cx.global::<Self>()
            .windows
            .iter()
            .find(|w| w.handle == handle)
            .and_then(|w| w.app.upgrade())
    }

    pub fn app_for(cx: &mut App, workspace: WorkspaceId) -> Option<WeakEntity<Tty7App>> {
        Self::sweep(cx);
        cx.global::<Self>()
            .windows
            .iter()
            .find(|w| w.workspace == workspace)
            .map(|w| w.app.clone())
    }

    fn register(
        cx: &mut App,
        workspace: WorkspaceId,
        handle: AnyWindowHandle,
        app: WeakEntity<Tty7App>,
    ) {
        cx.global_mut::<Self>().windows.push(WindowEntry {
            workspace,
            handle,
            app,
        });
    }

    pub fn unregister(cx: &mut App, workspace: WorkspaceId) {
        cx.global_mut::<Self>()
            .windows
            .retain(|w| w.workspace != workspace);
    }

    pub fn rebind(cx: &mut App, from: WorkspaceId, to: WorkspaceId) {
        if let Some(entry) = cx
            .global_mut::<Self>()
            .windows
            .iter_mut()
            .find(|w| w.workspace == from)
        {
            entry.workspace = to;
        }
    }

    fn sweep(cx: &mut App) {
        let dead: Vec<WorkspaceId> = cx
            .global::<Self>()
            .windows
            .iter()
            .filter(|w| w.app.upgrade().is_none())
            .map(|w| w.workspace)
            .collect();
        if dead.is_empty() {
            return;
        }
        cx.global_mut::<Self>()
            .windows
            .retain(|w| !dead.contains(&w.workspace));
    }
}

pub fn open(cx: &mut App, workspace: Option<WorkspaceId>) {
    open_at(cx, workspace, None);
}

pub fn open_at(
    cx: &mut App,
    workspace: Option<WorkspaceId>,
    initial_cwd: Option<std::path::PathBuf>,
) {
    if let Some(id) = workspace
        && let Some(handle) = WindowRegistry::window_for(cx, id)
    {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
        return;
    }

    let options = window_options(cx, workspace);
    let mut created: Option<gpui::Entity<Tty7App>> = None;
    let opened = cx.open_window(options, |window, cx| {
        let app = cx.new(|cx| match initial_cwd.clone() {
            Some(cwd) => Tty7App::for_workspace_at(workspace, Some(cwd), window, cx),
            None => Tty7App::for_workspace(workspace, window, cx),
        });
        created = Some(app.clone());
        cx.new(|cx| Root::new(app, window, cx).bg(gpui::transparent_black()))
    });

    let handle = match opened {
        Ok(handle) => handle,
        Err(e) => {
            log::error!("failed to open window: {e}");
            return;
        }
    };
    let Some(app) = created else {
        log::error!("opened a window but its Tty7App was never built; not registering");
        return;
    };

    let id = app.read(cx).workspace;
    WindowRegistry::register(cx, id, handle.into(), app.downgrade());
    refresh_menu(cx);
}

pub fn open_from_cli(cx: &mut App, path: Option<std::path::PathBuf>) {
    // Only the GUI process knows which of its windows was focused most recently.
    // The daemon deliberately routes to a process, then leaves window selection
    // to this registry.
    let workspace = if path.is_some() {
        WindowRegistry::most_recent_local(cx)
    } else {
        WindowRegistry::most_recent(cx)
    };
    let Some(workspace) = workspace else {
        open_missing_cli_window_with(cx, path, open_at);
        return;
    };
    let Some(handle) = WindowRegistry::window_for(cx, workspace) else {
        return;
    };
    let Some(app) = WindowRegistry::app_for(cx, workspace).and_then(|app| app.upgrade()) else {
        return;
    };

    cx.activate(true);
    let _ = handle.update(cx, move |_, window, cx| {
        if let Some(path) = path {
            app.update(cx, |app, cx| app.new_tab_at(path, window, cx));
        }
        window.activate_window();
    });
}

/// Opens a window after CLI routing reaches a GUI process with no live windows.
///
/// A pathless request follows the same restoration policy as normal startup.
/// An explicit path always starts a fresh local workspace so the requested tab
/// cannot accidentally be attached to a detached remote workspace.
fn open_missing_cli_window_with(
    cx: &mut App,
    path: Option<std::path::PathBuf>,
    open: impl FnOnce(&mut App, Option<WorkspaceId>, Option<std::path::PathBuf>),
) {
    let restore = path
        .is_none()
        .then(|| WorkspaceStore::restore_one(cx))
        .flatten();
    open(cx, restore, path);
}

pub fn refresh_menu(cx: &mut App) {
    crate::ui::theme::set_menus(cx);
}

pub const MENU_SLOTS: usize = 9;

pub fn menu_order(cx: &App) -> Vec<(WorkspaceId, bool)> {
    let all = WorkspaceStore::all(cx);
    let mut open: Vec<_> = all.views.iter().filter(|w| w.open).collect();
    let mut closed: Vec<_> = all.views.iter().filter(|w| !w.open).collect();
    open.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    closed.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    open.into_iter()
        .map(|w| (w.id, true))
        .chain(closed.into_iter().map(|w| (w.id, false)))
        .take(MENU_SLOTS)
        .collect()
}

pub struct PaneCountQuery {
    route: crate::terminal::PaneRoute,
    claimed: Vec<u64>,
}

pub fn pane_count_query(cx: &App, workspace: WorkspaceId) -> Option<PaneCountQuery> {
    let ws = WorkspaceStore::all(cx).get(workspace)?;
    Some(PaneCountQuery {
        route: crate::ui::remote_workspace::pane_route_for(cx, workspace),
        claimed: crate::ui::machine_mirror::pane_ids(cx, ws)?,
    })
}

pub fn live_pane_count(q: &PaneCountQuery) -> Option<usize> {
    let PaneCountQuery { route, claimed } = q;
    if claimed.is_empty() {
        return Some(0);
    }
    match crate::terminal::RemoteTerminal::try_list_panes_on(route) {
        Ok(panes) => {
            let alive: std::collections::HashSet<u64> = panes
                .into_iter()
                .filter(|p| p.alive)
                .map(|p| p.pane_id)
                .collect();
            Some(claimed.iter().filter(|id| alive.contains(id)).count())
        }
        Err(_) if matches!(route, crate::terminal::PaneRoute::Local) => Some(0),
        Err(_) => None,
    }
}

pub fn confirm_and_stop(cx: &mut App, window: &mut Window, workspace: WorkspaceId) {
    confirm_destructive(cx, window, workspace, "Stop", stop_workspace);
}

pub fn confirm_and_delete(cx: &mut App, window: &mut Window, workspace: WorkspaceId) {
    confirm_destructive(cx, window, workspace, "Delete", delete_workspace);
}

fn destructive_detail(live: Option<usize>, verb: &str) -> String {
    match (live, verb) {
        (None, "Delete") => "Its machine could not be reached. Any shells still running there \
                             will be ended, and the layout forgotten."
            .to_string(),
        (None, _) => {
            "Its machine could not be reached. Any shells still running there will be ended."
                .to_string()
        }
        (Some(0), _) => "Its layout and working directories will be forgotten.".to_string(),
        (Some(1), "Delete") => {
            "1 running shell will be ended and its layout forgotten.".to_string()
        }
        (Some(n), "Delete") => {
            format!("{n} running shells will be ended and the layout forgotten.")
        }
        (Some(1), _) => "1 running shell will be ended.".to_string(),
        (Some(n), _) => format!("{n} running shells will be ended."),
    }
}

fn confirm_destructive(
    cx: &mut App,
    window: &mut Window,
    workspace: WorkspaceId,
    verb: &'static str,
    act: fn(&mut App, WorkspaceId),
) {
    let name = crate::ui::machine_mirror::display_name_for(cx, workspace)
        .unwrap_or_else(|| "this workspace".to_string());
    let query = pane_count_query(cx, workspace);
    let handle = window.window_handle();

    cx.spawn(async move |cx| {
        let live = match query {
            Some(q) => {
                cx.background_spawn(async move { live_pane_count(&q) })
                    .await
            }
            None => None,
        };

        if live == Some(0) && verb == "Stop" {
            let _ = cx.update(|cx| act(cx, workspace));
            return;
        }

        let detail = destructive_detail(live, verb);
        let Ok(answer) = handle.update(cx, |_, window, cx| {
            window.prompt(
                gpui::PromptLevel::Warning,
                &format!("{verb} Workspace \u{201c}{name}\u{201d}?"),
                Some(&detail),
                &["Cancel", verb],
                cx,
            )
        }) else {
            return;
        };

        if let Ok(1) = answer.await {
            let _ = cx.update(|cx| act(cx, workspace));
        }
    })
    .detach();
}

pub fn stop_workspace(cx: &mut App, workspace: WorkspaceId) {
    let doomed = doomed_pane_ids(cx, workspace);
    stop_workspace_keeping(cx, workspace, doomed);
}

fn doomed_pane_ids(cx: &App, workspace: WorkspaceId) -> Vec<u64> {
    WorkspaceStore::all(cx)
        .get(workspace)
        .and_then(|ws| crate::ui::machine_mirror::pane_ids(cx, ws))
        .unwrap_or_default()
}

fn stop_workspace_keeping(cx: &mut App, workspace: WorkspaceId, ids: Vec<u64>) {
    let route = crate::ui::remote_workspace::pane_route_for(cx, workspace);
    let host = WorkspaceStore::all(cx)
        .get(workspace)
        .map(|w| w.host_id())
        .unwrap_or(crate::ui::host_ops::HostId::LOCAL);
    if !ids.is_empty() {
        let route = route.clone();
        cx.background_executor()
            .spawn(async move {
                for pane_id in ids {
                    crate::terminal::RemoteTerminal::kill_pane_on(&route, pane_id);
                }
            })
            .detach();
    }
    if cx
        .try_global::<crate::terminal::pane_liveness::PaneLivenessCache>()
        .is_some()
    {
        cx.update_global::<crate::terminal::pane_liveness::PaneLivenessCache, _>(|cache, _| {
            cache.invalidate(host)
        });
    }
    if let Some(app) = WindowRegistry::app_for(cx, workspace)
        && let Some(app) = app.upgrade()
    {
        app.read(cx).teardown_workspace_forwards(cx);
    }
    close_window_for(cx, workspace);
    WorkspaceStore::close_window(cx, workspace);
    refresh_menu(cx);
}

pub fn delete_workspace(cx: &mut App, workspace: WorkspaceId) {
    let doomed = delete_from_tree(cx, workspace);
    stop_workspace_keeping(cx, workspace, doomed);
    WorkspaceStore::remove(cx, workspace);
    release_unused_hosts(cx);
    refresh_menu(cx);
}

fn delete_from_tree(cx: &mut App, workspace: WorkspaceId) -> Vec<u64> {
    let doomed = doomed_pane_ids(cx, workspace);
    crate::ui::tree_sync::fire_workspace_op(cx, workspace, |ws| {
        tty7_core::daemon::control::ControlRequest::WorkspaceRemove { workspace: ws }
    });
    crate::ui::tree_sync::forget(cx, workspace);
    doomed
}

fn release_unused_hosts(cx: &mut App) {
    let live: Vec<_> = WorkspaceStore::all(cx)
        .views
        .iter()
        .filter(|w| w.is_remote())
        .map(|w| w.host_id())
        .collect();
    for id in crate::ui::host_registry::HostRegistry::ids(cx) {
        if !id.is_local() && !live.contains(&id) {
            crate::ui::remote_connect::HostLinks::remove(cx, id);
        }
    }
}

fn close_window_for(cx: &mut App, workspace: WorkspaceId) {
    let showing = WindowRegistry::app_for(cx, workspace);
    let Some(handle) = WindowRegistry::window_for(cx, workspace) else {
        return;
    };
    let Some(app) = showing.and_then(|weak| weak.upgrade()) else {
        return;
    };

    if WindowRegistry::count(cx) > 1 {
        WindowRegistry::unregister(cx, workspace);
        let _ = handle.update(cx, |_, window, _| window.remove_window());
        return;
    }

    let fresh = WorkspaceStore::claim(cx, None);
    WindowRegistry::rebind(cx, workspace, fresh);
    let _ = handle.update(cx, |_, window, cx| {
        app.update(cx, |app, cx| {
            app.adopt_workspace(fresh, crate::core::session::Session::default(), window, cx)
        });
    });
}

fn window_options(cx: &mut App, workspace: Option<WorkspaceId>) -> WindowOptions {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    static APP_ICON: std::sync::LazyLock<Option<std::sync::Arc<image::RgbaImage>>> =
        std::sync::LazyLock::new(|| {
            image::load_from_memory(include_bytes!("../../assets/app-icon.png"))
                .ok()
                .map(|image| std::sync::Arc::new(image.thumbnail(256, 256).into_rgba8()))
        });

    let remember = cx.global::<Config>().remember_window_size;
    let remembered = remember
        .then(|| {
            workspace
                .and_then(|id| WorkspaceStore::all(cx).get(id).and_then(|w| w.window))
                .or_else(WindowState::load)
        })
        .flatten();

    let existing = WindowRegistry::count(cx);
    let bounds = match remembered {
        Some(state) => {
            let bounds = state.bounds();
            if cx.displays().iter().any(|d| d.bounds().intersects(&bounds)) {
                bounds
            } else {
                Bounds::centered(None, bounds.size, cx)
            }
        }
        None => Bounds::centered(None, size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)), cx),
    };
    let bounds = cascade(bounds, existing);

    let window_bounds = match cx.global::<Config>().startup_mode {
        _ if existing > 0 => WindowBounds::Windowed(bounds),
        StartupMode::Normal => WindowBounds::Windowed(bounds),
        StartupMode::Maximized => WindowBounds::Maximized(bounds),
        StartupMode::Fullscreen => WindowBounds::Fullscreen(bounds),
    };

    WindowOptions {
        window_bounds: Some(window_bounds),
        app_id: Some("tty7".to_owned()),
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        icon: APP_ICON.as_ref().cloned(),
        titlebar: Some(TitlebarOptions {
            traffic_light_position: Some(crate::ui::theme::traffic_light_position()),
            ..TitleBar::title_bar_options()
        }),
        window_background: crate::ui::theme::background_appearance(cx),
        ..Default::default()
    }
}

fn cascade(bounds: Bounds<gpui::Pixels>, existing: usize) -> Bounds<gpui::Pixels> {
    if existing == 0 {
        return bounds;
    }
    let step = (existing % 5) as f32 * CASCADE_STEP;
    Bounds {
        origin: bounds.origin + point(px(step), px(step)),
        size: bounds.size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::session::{WindowView, WindowViews};

    fn bounds_at(x: f32, y: f32) -> Bounds<gpui::Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: size(px(800.), px(600.)),
        }
    }

    #[test]
    fn the_first_window_is_not_cascaded() {
        let b = bounds_at(100., 100.);
        assert_eq!(cascade(b, 0).origin, b.origin);
    }

    #[test]
    fn each_extra_window_steps_down_and_right() {
        let b = bounds_at(100., 100.);
        assert_eq!(
            cascade(b, 1).origin,
            point(px(100. + CASCADE_STEP), px(100. + CASCADE_STEP))
        );
        assert_eq!(
            cascade(b, 2).origin,
            point(px(100. + 2. * CASCADE_STEP), px(100. + 2. * CASCADE_STEP))
        );
        assert_eq!(cascade(b, 3).size, b.size);
    }

    #[test]
    fn cascade_wraps_so_windows_never_march_off_screen() {
        let b = bounds_at(100., 100.);
        assert_eq!(cascade(b, 5).origin, b.origin);
        assert_eq!(cascade(b, 6).origin, cascade(b, 1).origin);
    }

    #[gpui::test]
    fn a_pathless_cli_request_restores_a_workspace_when_no_window_is_open(
        cx: &mut gpui::TestAppContext,
    ) {
        let view = WindowView::default();
        let restored = view.id;
        let mut opened = None;

        cx.update(|cx| {
            WorkspaceStore::install_for_test(
                cx,
                WindowViews {
                    views: vec![view],
                    active: Some(restored),
                },
            );
            open_missing_cli_window_with(cx, None, |_, workspace, path| {
                opened = Some((workspace, path));
            });
        });

        assert_eq!(opened, Some((Some(restored), None)));
    }

    #[gpui::test]
    fn a_pathless_cli_request_creates_a_default_window_without_history(
        cx: &mut gpui::TestAppContext,
    ) {
        let mut opened = None;

        cx.update(|cx| {
            WorkspaceStore::install_for_test(cx, WindowViews::default());
            open_missing_cli_window_with(cx, None, |_, workspace, path| {
                opened = Some((workspace, path));
            });
        });

        assert_eq!(opened, Some((None, None)));
    }

    #[test]
    fn the_confirmation_says_which_of_the_three_answers_it_got() {
        assert_eq!(
            destructive_detail(Some(1), "Stop"),
            "1 running shell will be ended."
        );
        assert_eq!(
            destructive_detail(Some(3), "Stop"),
            "3 running shells will be ended."
        );
        assert_eq!(
            destructive_detail(Some(1), "Delete"),
            "1 running shell will be ended and its layout forgotten."
        );
        assert_eq!(
            destructive_detail(Some(3), "Delete"),
            "3 running shells will be ended and the layout forgotten."
        );
        assert_eq!(
            destructive_detail(Some(0), "Delete"),
            "Its layout and working directories will be forgotten."
        );

        for verb in ["Stop", "Delete"] {
            let detail = destructive_detail(None, verb);
            assert!(
                detail.contains("could not be reached"),
                "{verb}: {detail:?} must say why there is no count"
            );
            assert!(
                !detail.contains("forgotten.") || verb == "Delete",
                "{verb}: {detail:?} promises a delete-only consequence"
            );
            assert!(
                !detail.chars().any(|c| c.is_ascii_digit()),
                "{verb}: {detail:?} states a count it does not have"
            );
        }
    }

    #[gpui::test]
    fn a_delete_reads_its_kill_list_before_the_removal_blanks_the_mirror(
        cx: &mut gpui::TestAppContext,
    ) {
        use tty7_core::core::machine::{Machine, PaneRecord, Tab, Workspace as TreeWorkspace};

        cx.update(|cx| {
            let view = WindowView::default();
            let id = view.id;
            WorkspaceStore::install_for_test(
                cx,
                WindowViews {
                    views: vec![view],
                    active: None,
                },
            );
            crate::ui::machine_mirror::MachineMirrors::install(
                cx,
                crate::ui::host_ops::HostId::LOCAL,
                Machine {
                    workspaces: vec![TreeWorkspace {
                        id,
                        tabs: vec![Tab::leaf(1), Tab::leaf(2), Tab::leaf(3)],
                        ..TreeWorkspace::default()
                    }],
                    panes: vec![PaneRecord::new(1), PaneRecord::new(2), PaneRecord::new(3)],
                },
            );

            let doomed = delete_from_tree(cx, id);
            assert_eq!(
                doomed,
                vec![1, 2, 3],
                "every shell the confirm prompt counted must be on the kill list"
            );
            assert!(
                doomed_pane_ids(cx, id).is_empty(),
                "the removal has been folded into the mirror — which is exactly why \
                 the list must be read first"
            );
        });
    }
}

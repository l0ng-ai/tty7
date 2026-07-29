//! The app-level window registry, and the single place that opens a window.
//!
//! tty7 used to have exactly one window, so `main` opened it inline and every
//! app-wide duty (tray, menus, the quit hook) could live in `Tty7App`'s
//! constructor. With several windows those duties have to belong to the *app*,
//! and anything that acts on "a window" — a tray click, `New Workspace`, the quit
//! hook walking every open workspace — needs a way to find them. That is this
//! module.
//!
//! The registry maps each live window to the [`WorkspaceId`] it displays.
//! Windows are transient views; workspaces are the persistent identity
//! (`core::session`). Exactly one window shows a given workspace at a time —
//! the daemon gives each pane a single subscriber, so two windows attached to
//! one workspace would have the second silently steal the first's output.
//! [`open`] enforces that by focusing an already-open workspace instead of
//! opening a second window onto it.

use gpui::{
    AnyWindowHandle, App, AppContext as _, BorrowAppContext as _, Bounds, Global, Styled as _,
    TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions, point, px, size,
};
use gpui_component::{Root, TitleBar};

use crate::core::config::{Config, StartupMode};
use crate::core::session::{WorkspaceId, WorkspaceStore};
use crate::core::window_state::{WindowGeometry as _, WindowState};
use crate::ui::app::Tty7App;

/// How far each additional window is offset from the one before it, so a new
/// window never lands exactly on top of an existing one (logical px).
const CASCADE_STEP: f32 = 28.0;

/// Default size for a window with nothing remembered.
const DEFAULT_SIZE: (f32, f32) = (1440.0, 900.0);

/// One live window and what it is showing.
struct WindowEntry {
    workspace: WorkspaceId,
    handle: AnyWindowHandle,
    /// Weak so a closed window's entity can drop normally; a dead handle is
    /// pruned on the next sweep rather than keeping the app alive.
    app: WeakEntity<Tty7App>,
}

/// Every window tty7 currently has open.
#[derive(Default)]
pub struct WindowRegistry {
    windows: Vec<WindowEntry>,
}

impl Global for WindowRegistry {}

impl WindowRegistry {
    /// Install the empty registry. Call once, before the first window opens.
    pub fn init(cx: &mut App) {
        cx.set_global(Self::default());
    }

    /// Number of live windows. Drives "is this the last window?" — the check
    /// that decides whether closing one quits the app.
    pub fn count(cx: &mut App) -> usize {
        Self::sweep(cx);
        cx.global::<Self>().windows.len()
    }

    /// The workspaces currently on screen, with the entity to read their tabs
    /// from. Used by the quit hook to record every window's final state.
    pub fn open_windows(cx: &mut App) -> Vec<(WorkspaceId, WeakEntity<Tty7App>)> {
        Self::sweep(cx);
        cx.global::<Self>()
            .windows
            .iter()
            .map(|w| (w.workspace, w.app.clone()))
            .collect()
    }

    /// The window showing `workspace`, if one is open.
    pub fn window_for(cx: &mut App, workspace: WorkspaceId) -> Option<AnyWindowHandle> {
        Self::sweep(cx);
        cx.global::<Self>()
            .windows
            .iter()
            .find(|w| w.workspace == workspace)
            .map(|w| w.handle)
    }

    /// The workspace of the most recently focused window — the sensible target
    /// for an app-wide action (a tray click, "open Settings") that needs *a*
    /// window but doesn't care which. Falls back to the first live window when
    /// the store has no opinion.
    pub fn most_recent(cx: &mut App) -> Option<WorkspaceId> {
        Self::sweep(cx);
        let active = WorkspaceStore::all(cx).active;
        let registry = cx.global::<Self>();
        active
            .filter(|id| registry.windows.iter().any(|w| w.workspace == *id))
            .or_else(|| registry.windows.first().map(|w| w.workspace))
    }

    /// The `Tty7App` showing `workspace`, if one is open.
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

    /// Forget a window. Idempotent — a window can be dropped by its own close
    /// path and then swept again when its entity finally releases.
    pub fn unregister(cx: &mut App, workspace: WorkspaceId) {
        cx.global_mut::<Self>()
            .windows
            .retain(|w| w.workspace != workspace);
    }

    /// Point an existing window at a different workspace, keeping its handle
    /// and entity. Used when the picker swaps a window's contents in place
    /// rather than opening a second window (see `Tty7App::switch_workspace`).
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

    /// Drop entries whose `Tty7App` entity is gone. Windows can close through
    /// paths that never reach our own teardown (an OS-level close, a panic in a
    /// sibling view), so every read prunes first rather than trusting the list.
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

/// What a *brand-new* workspace's window starts with. Only consulted when the
/// window is opening on a freshly minted workspace — one restored from
/// `session.json` always rebuilds its saved tabs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FreshStart {
    /// A single default terminal, the way every previous launch of tty7 came
    /// up. What `New Workspace` and a genuine first run want: a window whose
    /// workspace has nothing in it yet is a window you asked for to work in.
    Shell,
    /// No tabs — the home page. Used at launch when there *are* saved
    /// workspaces but none were open at quit: the picker listing them is the
    /// whole point of that window, and a shell in front of it would bury it.
    HomePage,
}

/// Open a window on `workspace` — or on a brand-new workspace when `None`,
/// which starts with a single terminal (see [`open_with`] for the other case).
///
/// When that workspace already has a window, this focuses it instead of
/// opening a second one: two windows on one workspace would both attach the
/// same daemon panes, and the daemon's single-subscriber model means the
/// second attach silently kills the first window's terminal.
pub fn open(cx: &mut App, workspace: Option<WorkspaceId>) {
    open_with(cx, workspace, FreshStart::Shell);
}

/// [`open`], with a say in what a brand-new workspace comes up holding.
pub fn open_with(cx: &mut App, workspace: Option<WorkspaceId>, fresh: FreshStart) {
    if let Some(id) = workspace
        && let Some(handle) = WindowRegistry::window_for(cx, id)
    {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
        return;
    }

    let options = window_options(cx, workspace);
    // The registry needs the window's `Tty7App`, but `open_window` hands back
    // only the root view — so capture it on the way past.
    let mut created: Option<gpui::Entity<Tty7App>> = None;
    let opened = cx.open_window(options, |window, cx| {
        let app = cx.new(|cx| Tty7App::for_workspace(workspace, fresh, window, cx));
        created = Some(app.clone());
        // Root's own background is fully transparent: `Tty7App`'s root div is
        // the single owner of the window background (solid / gradient / image,
        // with the theme's alpha). A second paint here would compound the alpha
        // and read darker than the configured opacity.
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

    // Read back the workspace the window actually claimed — passing `None`
    // mints a fresh one, so the caller's id isn't authoritative.
    let id = app.read(cx).workspace;
    WindowRegistry::register(cx, id, handle.into(), app.downgrade());
    refresh_menu(cx);
}

/// Rebuild the menu bar so the Window menu reflects the current workspace set.
///
/// macOS menus are static snapshots — nothing re-reads them when they open —
/// so every change to *which* workspaces exist has to push a new one. Called
/// on open / detach / switch / end, but deliberately not on ordinary tab edits:
/// a workspace's name comes from its repo and effectively never changes, so
/// rebuilding the whole menu bar per tab would be churn for nothing.
pub fn refresh_menu(cx: &mut App) {
    crate::ui::theme::set_menus(cx);
}

/// Most workspaces listed in the Window menu. Nine because that is how many
/// `SelectWorkspace1..9` actions exist — the same ceiling the tab shortcuts
/// use, and past which a flat menu stops being scannable anyway.
pub const MENU_SLOTS: usize = 9;

/// The Window menu's ordering, shared by the menu builder and the actions that
/// index into it so slot *n* always means the same workspace in both.
///
/// Open windows first (this is the macOS Window menu — its primary job is
/// listing what is on screen), then detached workspaces most-recent-first. That
/// second group is the whole point: a workspace closed with ⌘W has to be
/// visible *somewhere* or it may as well have been deleted.
pub fn menu_order(cx: &App) -> Vec<(WorkspaceId, bool)> {
    let all = WorkspaceStore::all(cx);
    let mut open: Vec<_> = all.workspaces.iter().filter(|w| w.open).collect();
    let mut closed: Vec<_> = all.workspaces.iter().filter(|w| !w.open).collect();
    open.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    closed.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    open.into_iter()
        .map(|w| (w.id, true))
        .chain(closed.into_iter().map(|w| (w.id, false)))
        .take(MENU_SLOTS)
        .collect()
}

/// How many of a workspace's panes are still running on its own machine.
/// `Some(0)` means closing it destroys nothing — every shell already exited —
/// so the caller can skip the confirmation prompt.
///
/// `None` is "the machine could not be asked", and it exists because the old
/// `0` conflated the two. A remote workspace whose link was down counted zero
/// live panes, and "Stop Workspace" then went through **without a prompt** and
/// killed sessions the user was never told about. Only a machine that answered
/// can license skipping the confirmation.
///
/// Answered synchronously rather than from
/// [`pane_liveness`](crate::terminal::pane_liveness): the prompt states an exact
/// number about an irreversible action, so it wants a fresh count, not one that
/// may be ten seconds old. This runs on a click, not on a frame.
/// What [`live_pane_count`] needs from the app, gathered on the UI thread so the
/// count itself does not have to run there.
pub struct PaneCountQuery {
    route: crate::terminal::PaneRoute,
    claimed: Vec<u64>,
}

/// Read the inputs for [`live_pane_count`]. Cheap; UI thread only.
pub fn pane_count_query(cx: &App, workspace: WorkspaceId) -> Option<PaneCountQuery> {
    let ws = WorkspaceStore::all(cx).get(workspace)?;
    Some(PaneCountQuery {
        // Routed to the workspace's own machine: a remote workspace's pane ids
        // mean nothing to this computer's daemon, so asking it would count
        // whichever *local* panes happen to hold those numbers and put a "3
        // running sessions will be ended" warning on a workspace that has none.
        route: crate::ui::remote_workspace::pane_route_for(cx, workspace),
        claimed: ws.pane_ids(),
    })
}

/// **Blocking. Never call this on the UI thread.**
///
/// For a remote route this dials the workspace's machine — an SSH handshake if
/// nothing is pooled — and a WSL one can go as far as installing the server
/// binary. `guard_off_ui` makes a UI-thread call a debug-build abort rather
/// than a dropped frame, which is what it did when this was reached straight
/// from the Stop/Delete action handler.
pub fn live_pane_count(q: &PaneCountQuery) -> Option<usize> {
    let PaneCountQuery { route, claimed } = q;
    if claimed.is_empty() {
        return Some(0);
    }
    // One short-lived connection, only when there is something to ask about —
    // the picker renders far more often than a workspace is closed.
    match crate::terminal::RemoteTerminal::try_list_panes_on(route) {
        Ok(panes) => {
            let alive: std::collections::HashSet<u64> = panes
                .into_iter()
                .filter(|p| p.alive)
                .map(|p| p.pane_id)
                .collect();
            Some(claimed.iter().filter(|id| alive.contains(id)).count())
        }
        // This machine is the one case where a refused `List` *is* an answer:
        // the daemon lives at a known socket on the same box, and one that
        // cannot be reached is one with nothing running in it. Keeping this
        // arm is what makes a local workspace behave exactly as it did before
        // any of this was routed.
        Err(_) if matches!(route, crate::terminal::PaneRoute::Local) => Some(0),
        Err(_) => None,
    }
}

/// Confirm, then stop `workspace`. Skips the prompt when nothing is running —
/// there is nothing to lose and it would be pure friction.
pub fn confirm_and_stop(cx: &mut App, window: &mut Window, workspace: WorkspaceId) {
    confirm_destructive(cx, window, workspace, "Stop", stop_workspace);
}

/// Confirm, then delete `workspace`. Always asks: even with every shell
/// already exited, the saved layout is still something to lose.
pub fn confirm_and_delete(cx: &mut App, window: &mut Window, workspace: WorkspaceId) {
    confirm_destructive(cx, window, workspace, "Delete", delete_workspace);
}

/// What the confirmation prompt says below its title, given what
/// [`live_pane_count`] found.
///
/// Split out of [`confirm_destructive`] because it is the one part of a
/// `window.prompt` path that can be tested: the three answers a liveness query
/// can give — a count, zero, and "could not ask" — each have to reach the user
/// as a different sentence, and the third one is new. Getting it wrong is not a
/// wording bug: it is telling somebody nothing will be lost right before ending
/// their sessions.
fn destructive_detail(live: Option<usize>, verb: &str) -> String {
    match (live, verb) {
        // The machine could not be asked, so no number can be promised — say
        // what is actually known, which is that anything still running there
        // is about to end.
        (None, "Delete") => "Its machine could not be reached. Any sessions still running there \
                             will be ended, and the layout forgotten."
            .to_string(),
        (None, _) => {
            "Its machine could not be reached. Any sessions still running there will be ended."
                .to_string()
        }
        (Some(0), _) => "Its layout and working directories will be forgotten.".to_string(),
        (Some(1), "Delete") => {
            "1 running session will be ended and its layout forgotten.".to_string()
        }
        (Some(n), "Delete") => {
            format!("{n} running sessions will be ended and the layout forgotten.")
        }
        (Some(1), _) => "1 running session will be ended.".to_string(),
        (Some(n), _) => format!("{n} running sessions will be ended."),
    }
}

/// Shared confirm-then-act path for the two destructive workspace actions.
///
/// A free function rather than a `Tty7App` method because the title-bar menu's
/// row buttons run inside a menu builder, which has a `Window` and an `App` but
/// no entity to call a method on.
fn confirm_destructive(
    cx: &mut App,
    window: &mut Window,
    workspace: WorkspaceId,
    verb: &'static str,
    act: fn(&mut App, WorkspaceId),
) {
    let name = WorkspaceStore::all(cx)
        .get(workspace)
        .map(|w| w.display_name())
        .unwrap_or_else(|| "this workspace".to_string());
    let query = pane_count_query(cx, workspace);
    let handle = window.window_handle();

    cx.spawn(async move |cx| {
        // The count dials the workspace's machine, so it does not belong on the
        // UI thread — on a remote route that is an SSH handshake, and on a WSL
        // one it can go as far as installing the server. Reached straight from
        // the action handler, it was a `guard_off_ui` abort in a debug build
        // and a window frozen for the length of a connect in a release one.
        let live = match query {
            Some(q) => {
                cx.background_spawn(async move { live_pane_count(&q) })
                    .await
            }
            None => None,
        };

        // Only a machine that *answered* zero licenses skipping the prompt. An
        // unreachable one is the case most likely to still have work in it.
        if live == Some(0) && verb == "Stop" {
            let _ = cx.update(|cx| act(cx, workspace));
            return;
        }

        let detail = destructive_detail(live, verb);
        // Title Case, like every other prompt title in the app — this one used
        // to lowercase "workspace" while its siblings read "Close Window?" /
        // "Quit and Stop Daemon?".
        let Ok(answer) = handle.update(cx, |_, window, cx| {
            window.prompt(
                gpui::PromptLevel::Warning,
                &format!("{verb} Workspace \u{201c}{name}\u{201d}?"),
                Some(&detail),
                &["Cancel", verb],
                cx,
            )
        }) else {
            // The window went away while we were asking its machine. Nothing to
            // confirm against, and acting unprompted is exactly what this path
            // exists to prevent.
            return;
        };

        // Index 1 == the verb button; Cancel and a dismissed prompt both leave
        // the workspace alone.
        if let Ok(1) = answer.await {
            let _ = cx.update(|cx| act(cx, workspace));
        }
    })
    .detach();
}

/// Stop a workspace: kill every pane it owns in the daemon, and close the
/// window showing it.
///
/// The workspace *record* survives — its tabs, split layout and each pane's cwd
/// stay on file — so reopening it later rebuilds the same arrangement with
/// fresh shells. That is the difference from [`delete_workspace`], which throws
/// the record away too.
///
/// Callers confirm first unless [`live_pane_count`] answered zero; with nothing
/// running there is nothing to lose.
pub fn stop_workspace(cx: &mut App, workspace: WorkspaceId) {
    stop_workspace_keeping(cx, workspace, ClearedLayout::Push);
}

/// What to do with the record once its panes are dead.
#[derive(Clone, Copy, PartialEq)]
enum ClearedLayout {
    /// Send it to the machine that owns it — the workspace is going to be
    /// reopened, and it must not reopen claiming panes that no longer exist.
    Push,
    /// Leave it alone: the caller is about to delete the record outright, and a
    /// push racing that delete could put the workspace back on the machine.
    Discard,
}

fn stop_workspace_keeping(cx: &mut App, workspace: WorkspaceId, cleared: ClearedLayout) {
    // A remote workspace's panes live on the remote server, and its pane ids are
    // *that* daemon's. Sending them here would not fail — it would succeed
    // against whatever local panes happen to hold those numbers, killing a
    // stranger's shells. The route is what makes "Stop" mean the same thing on
    // both kinds of workspace.
    let route = crate::ui::remote_workspace::pane_route_for(cx, workspace);
    let host = WorkspaceStore::all(cx)
        .get(workspace)
        .map(|w| w.host_id())
        .unwrap_or(crate::ui::host_ops::HostId::LOCAL);
    let ids = WorkspaceStore::all(cx)
        .get(workspace)
        .map(|ws| ws.pane_ids())
        .unwrap_or_default();
    if !ids.is_empty() {
        // Off the UI thread: each of these dials `route`, and on a remote
        // workspace that is an SSH channel per pane. Stopping a four-pane
        // workspace used to freeze the window for as long as four round trips.
        // Fire-and-forget — a missing daemon means there was nothing to kill.
        let route = route.clone();
        cx.background_executor()
            .spawn(async move {
                for pane_id in ids {
                    crate::terminal::RemoteTerminal::kill_pane_on(&route, pane_id);
                }
            })
            .detach();
    }
    // The panes this machine just reported as alive are the ones we killed, so
    // the cached answer is now wrong by our own hand. Waiting out its TTL would
    // leave a green dot on the picker row of a workspace the user just stopped.
    if cx
        .try_global::<crate::terminal::pane_liveness::PaneLivenessCache>()
        .is_some()
    {
        cx.update_global::<crate::terminal::pane_liveness::PaneLivenessCache, _>(|cache, _| {
            cache.invalidate(host)
        });
    }
    // A remote workspace's port forwards are owned by the
    // *workspace*, not by its panes, so nothing else ends them. Done before the
    // window closes, because the route to the daemon is read off a live pane.
    if let Some(app) = WindowRegistry::app_for(cx, workspace)
        && let Some(app) = app.upgrade()
    {
        app.read(cx).teardown_workspace_forwards(cx);
    }
    // One workspace is shown by exactly one window, so stopping the work means
    // the window goes with it — leaving an empty frame behind reads as a
    // half-finished action.
    close_window_for(cx, workspace);
    WorkspaceStore::close_window(cx, workspace);
    // Last, and after the window is gone so nothing records the old layout back
    // over it: the ids we just killed are dead by our own hand, and a record
    // that still claims them reopens into panes that cannot be attached to.
    // Locally that is invisible (`alive_panes_on` asks the daemon and gets the
    // same answer); on a remote workspace nobody asks, so the stale id is the
    // whole difference between reopening onto fresh shells with the agent
    // conversation resumed and reopening onto `tty7 — disconnected`.
    forget_killed_panes(cx, workspace, cleared);
    refresh_menu(cx);
}

/// Drop `workspace`'s pane ids, and tell the machine that owns the record.
///
/// The push is not optional for a remote workspace that is being kept: design
/// The remote's `workspaces.json` is the authority, so reopening pulls
/// its copy over the client's ([`WorkspaceStore::apply_remote`]) and a
/// local-only edit would be undone by the next open — which is the open this
/// exists for.
fn forget_killed_panes(cx: &mut App, workspace: WorkspaceId, cleared: ClearedLayout) {
    if !WorkspaceStore::forget_pane_ids(cx, workspace) {
        return;
    }
    if cleared == ClearedLayout::Discard {
        return;
    }
    let Some((host, key, record)) = WorkspaceStore::remote_payload(cx, workspace) else {
        return;
    };
    let Some(connection) = crate::ui::remote_workspace::connection_for(cx, workspace) else {
        // Not connected, so the panes were not killed either — `kill_pane_on`
        // needs the same route. The client's copy is still worth clearing: it
        // is what a reconnect pushes back up.
        log::info!(
            "ended sessions on {} without reaching it; the cleared layout goes up on reconnect",
            host.target
        );
        return;
    };
    cx.background_executor()
        .spawn(async move {
            if let Err(e) = crate::ui::remote_connect::put_remote_layout(&connection, key, record) {
                log::warn!(
                    "could not tell {} its workspace's panes are gone: {e}",
                    host.target
                );
            }
        })
        .detach();
}

/// Delete a workspace outright: stop it, then forget it entirely. Irreversible
/// — nothing about the layout survives.
pub fn delete_workspace(cx: &mut App, workspace: WorkspaceId) {
    // Delete it on the machine that owns it first, while the pointer to it is
    // still on file. Doing this after `WorkspaceStore::remove` would leave the
    // record stranded on the remote with no way left to name it.
    delete_on_remote(cx, workspace);
    // …and the stop that follows must not push the emptied layout back up: the
    // delete above is in flight on a background task, and a push landing after
    // it would recreate the record it just removed.
    stop_workspace_keeping(cx, workspace, ClearedLayout::Discard);
    WorkspaceStore::remove(cx, workspace);
    release_unused_hosts(cx);
    refresh_menu(cx);
}

/// Forget a remote workspace on the machine that owns it (the
/// remote's `workspaces.json` is the authority, so deleting only the client's
/// pointer would leave the workspace there and reappear on the next connect).
///
/// A no-op for a local workspace, and for a remote one this client is not
/// currently connected to — there is no way to reach the record, and the delete
/// is a user action rather than something to queue and replay later.
fn delete_on_remote(cx: &mut App, workspace: WorkspaceId) {
    let Some(host) = WorkspaceStore::remote_ref(cx, workspace) else {
        return;
    };
    let Some(connection) = crate::ui::remote_workspace::connection_for(cx, workspace) else {
        log::info!(
            "deleting the local pointer to a workspace on {} without reaching it",
            host.target
        );
        return;
    };
    let key = host.store_key();
    cx.background_executor()
        .spawn(async move {
            if let Err(e) = crate::ui::remote_connect::delete_remote_workspace(&connection, key) {
                log::warn!("could not delete the workspace on {}: {e}", host.target);
            }
        })
        .detach();
}

/// Drop the connection to any machine no workspace points at any more.
///
/// One connection per machine is shared by every workspace on it, so it is
/// released when the *last* one goes — not when a window closes. Anything less
/// careful would tear down a live sibling window's host mid-call.
fn release_unused_hosts(cx: &mut App) {
    let live: Vec<_> = WorkspaceStore::all(cx)
        .workspaces
        .iter()
        .filter(|w| w.is_remote())
        .map(|w| w.host_id())
        .collect();
    for id in crate::ui::host_registry::HostRegistry::ids(cx) {
        if !id.is_local() && !live.contains(&id) {
            crate::ui::remote_connect::RemoteConnections::remove(cx, id);
        }
    }
}

/// Close whichever window is showing `workspace`, if any.
///
/// The last window is the exception: it stays, swapped onto a fresh blank
/// workspace, because a windowless tty7 left in the Dock stops responding to
/// clicks (#147).
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

    let (fresh, session) = WorkspaceStore::claim(cx, None);
    WindowRegistry::rebind(cx, workspace, fresh);
    let _ = handle.update(cx, |_, window, cx| {
        app.update(cx, |app, cx| {
            app.adopt_workspace(fresh, session, window, cx)
        });
    });
}

/// Where a new window should appear: the workspace's own remembered geometry
/// first (that is where the user left *this* workspace), then the shared
/// `window.json` fallback, then a centred default — each cascaded so it does
/// not land exactly on an existing window.
fn window_options(cx: &mut App, workspace: Option<WorkspaceId>) -> WindowOptions {
    // X11 needs the icon on the native window itself for taskbars and window
    // switchers. Wayland resolves the same application identity through the
    // desktop entry when tty7 is packaged.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    static APP_ICON: std::sync::LazyLock<Option<std::sync::Arc<image::RgbaImage>>> =
        std::sync::LazyLock::new(|| {
            image::load_from_memory(include_bytes!("../../assets/app-icon.png"))
                .ok()
                // The source asset is 1024×1024, but _NET_WM_ICON ships raw
                // pixels to the X server per window (~4 MB at full size) and
                // taskbars want at most 256px anyway.
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
        // A remembered window that no longer touches any display (monitor
        // unplugged, resolution change) keeps its size but re-centers.
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

    // Launch state from config: a normal window, or maximized / fullscreen.
    // Each variant still carries the bounds above as the size to restore to
    // when the user un-maximizes / exits fullscreen. Only the *first* window
    // honors maximized/fullscreen — a second window forced fullscreen would
    // hide the one the user was just in.
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
        // Start from the component defaults but nudge the traffic lights down
        // so they stay vertically centred in our taller (40px) title bar — see
        // `TitleBar::new().h(..)` in `app.rs`. `apply_theme` re-pins the same
        // position after appearance changes.
        titlebar: Some(TitlebarOptions {
            traffic_light_position: Some(crate::ui::theme::traffic_light_position()),
            ..TitleBar::title_bar_options()
        }),
        // Non-opaque from creation: macOS 26 ignores a runtime flip to
        // transparent, so the opacity slider only works on a window born this
        // way (see `theme::background_appearance`).
        window_background: crate::ui::theme::background_appearance(cx),
        ..Default::default()
    }
}

/// Offset `bounds` by one cascade step per existing window, so opening several
/// windows in a row doesn't stack them invisibly on top of each other.
fn cascade(bounds: Bounds<gpui::Pixels>, existing: usize) -> Bounds<gpui::Pixels> {
    if existing == 0 {
        return bounds;
    }
    // Wrap after a few steps so a long-lived session doesn't march windows off
    // the bottom-right of the display.
    let step = (existing % 5) as f32 * CASCADE_STEP;
    Bounds {
        origin: bounds.origin + point(px(step), px(step)),
        size: bounds.size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Size is never touched — only the origin moves.
        assert_eq!(cascade(b, 3).size, b.size);
    }

    #[test]
    fn cascade_wraps_so_windows_never_march_off_screen() {
        let b = bounds_at(100., 100.);
        // The 5th extra window is back at the un-offset origin rather than
        // 5 steps further down-right.
        assert_eq!(cascade(b, 5).origin, b.origin);
        assert_eq!(cascade(b, 6).origin, cascade(b, 1).origin);
    }

    /// The three answers a liveness query can give each reach the user as a
    /// different sentence — and the counted ones read exactly as they did
    /// before "could not ask" became expressible.
    #[test]
    fn the_confirmation_says_which_of_the_three_answers_it_got() {
        // Counted: unchanged wording, singular and plural, stop and delete.
        assert_eq!(
            destructive_detail(Some(1), "Stop"),
            "1 running session will be ended."
        );
        assert_eq!(
            destructive_detail(Some(3), "Stop"),
            "3 running sessions will be ended."
        );
        assert_eq!(
            destructive_detail(Some(1), "Delete"),
            "1 running session will be ended and its layout forgotten."
        );
        assert_eq!(
            destructive_detail(Some(3), "Delete"),
            "3 running sessions will be ended and the layout forgotten."
        );
        // Counted zero: nothing is running, so only the layout is at stake.
        assert_eq!(
            destructive_detail(Some(0), "Delete"),
            "Its layout and working directories will be forgotten."
        );

        // Could not ask. It must not claim a number, must not claim nothing
        // will be lost, and must name the reason.
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
}

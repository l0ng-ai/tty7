//! The gpui-facing half of session persistence.
//!
//! The on-disk model — [`SessionPane`], [`SessionTab`], [`Session`],
//! [`Workspace`], [`Workspaces`] and all the `session.json` IO — lives in
//! `tty7-core`: it is pure serde, and the remote server has to read and write
//! the identical file. What is left here is [`WorkspaceStore`], which is a gpui
//! `Global` and threads every mutation through `&mut App`.

pub use tty7_core::core::session::{
    RemoteRef, RemoteTarget, Session, SessionAxis, SessionPane, SessionTab, Workspace, WorkspaceId,
    Workspaces,
};
pub use tty7_core::host::HostId;

/// App-level owner of `session.json`, and the single writer to it.
///
/// Windows never touch the file themselves. Each one pushes *its* workspace's
/// state in and the store persists the merged whole — without that, two windows
/// doing read-modify-write on the shared file would have the last writer
/// clobber the other's tabs. It also means a window that is closing can record
/// its final state after its own entity is already being torn down.
pub struct WorkspaceStore {
    workspaces: Workspaces,
}

impl gpui::Global for WorkspaceStore {}

impl WorkspaceStore {
    /// Read `session.json` (migrating a legacy flat session), drop any
    /// duplicate pane claims, and install the result as the app global. Call
    /// once, before the first window is built.
    pub fn init(cx: &mut gpui::App) {
        let workspaces = Workspaces::load().unwrap_or_default();
        cx.set_global(Self { workspaces });
    }

    /// Install a store holding exactly `workspaces`.
    ///
    /// Tests only, and it exists because [`init`](Self::init) reads the
    /// developer's real `session.json`: a test that needs a workspace to be on
    /// file must neither depend on what happens to be there nor risk writing to
    /// it. Every mutating helper already no-ops without the global, so this is
    /// the one thing a test cannot do for itself.
    #[cfg(test)]
    pub fn install_for_test(cx: &mut gpui::App, workspaces: Workspaces) {
        cx.set_global(Self { workspaces });
    }

    /// Every known workspace. Read-only — mutations go through the helpers so
    /// the file stays in step.
    ///
    /// Reads as empty when the store was never installed. That is the headless
    /// test harness, which builds windows directly rather than through
    /// `ui::windows::open`; "no saved workspaces" is the correct reading there,
    /// and it keeps a missing global from panicking a render.
    pub fn all(cx: &gpui::App) -> &Workspaces {
        static EMPTY: std::sync::OnceLock<Workspaces> = std::sync::OnceLock::new();
        match cx.try_global::<Self>() {
            Some(store) => &store.workspaces,
            None => EMPTY.get_or_init(Workspaces::default),
        }
    }

    /// The store, or `None` when it was never installed (tests). Every mutating
    /// helper goes through this so a headless window is a no-op rather than a
    /// panic — and, importantly, so tests never write to a real `session.json`.
    fn try_store(cx: &mut gpui::App) -> Option<&mut Self> {
        cx.has_global::<Self>().then(|| cx.global_mut::<Self>())
    }

    /// Take over an existing workspace to show in a window, or mint a fresh one
    /// when `id` is `None` / no longer on file (the "New Workspace" path). Marks it
    /// open and returns its id plus the tabs the window should rebuild.
    pub fn claim(cx: &mut gpui::App, id: Option<WorkspaceId>) -> (WorkspaceId, Session) {
        // Read before the store is borrowed: whether the layout may be rebuilt
        // depends on another global (the connection table), and a remote
        // workspace whose machine is unreachable must open empty. See
        // [`claimable_session`].
        let reachable = id.is_none_or(|id| Self::machine_is_connected(cx, id));
        let Some(store) = Self::try_store(cx) else {
            // No store (tests): hand back a detached identity so the window
            // still builds, but nothing is persisted.
            return (WorkspaceId::new(), Session::default());
        };
        let id = id.filter(|id| store.workspaces.get(*id).is_some());
        let workspace = match id {
            Some(id) => store.workspaces.get_mut(id).expect("filtered above"),
            None => {
                store.workspaces.workspaces.push(Workspace::default());
                store.workspaces.workspaces.last_mut().expect("just pushed")
            }
        };
        workspace.open = true;
        workspace.touch();
        let claimed = (workspace.id, claimable_session(workspace, reachable));
        store.workspaces.active = Some(claimed.0);
        store.workspaces.save();
        claimed
    }

    /// Record a window's current tabs (and geometry, when known) and persist.
    /// Called on every structural change, exactly where `Session::save` used to be.
    pub fn record(
        cx: &mut gpui::App,
        id: WorkspaceId,
        session: Session,
        window: Option<crate::core::window_state::WindowState>,
    ) {
        // Same reason as in [`claim`]: read the connection table before the
        // store is borrowed. A window whose machine is unreachable is not
        // describing that machine's layout, so it does not get to overwrite the
        // copy we have of it — see [`record_session`].
        let reachable = Self::machine_is_connected(cx, id);
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        let Some(workspace) = store.workspaces.get_mut(id) else {
            // The workspace was closed out from under us (its window is
            // tearing down); nothing to record.
            return;
        };
        record_session(workspace, session, reachable);
        if let Some(window) = window {
            workspace.window = Some(window);
        }
        store.workspaces.save();
    }

    /// Mark the focused workspace, so the next launch restores focus to the
    /// window the user was actually in.
    pub fn focus(cx: &mut gpui::App, id: WorkspaceId) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        if let Some(workspace) = store.workspaces.get_mut(id) {
            workspace.touch();
        }
        store.workspaces.active = Some(id);
        store.workspaces.save();
        // The machine's tree keeps its own recency (its pickers order by it),
        // so the focus is a fact to report there too.
        crate::ui::tree_sync::fire_workspace_op(cx, id, |ws| {
            tty7_core::daemon::control::ControlRequest::WorkspaceTouch { workspace: ws }
        });
    }

    /// Record a rename that *arrived from* the machine — the local half of
    /// [`rename`](Self::rename), without the operation that would echo it
    /// straight back.
    pub fn rename_locally(cx: &mut gpui::App, id: WorkspaceId, name: Option<String>) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        if let Some(workspace) = store.workspaces.get_mut(id) {
            workspace.name = name;
        }
        store.workspaces.save();
    }

    /// Set (or clear, with `None`) a workspace's user-chosen name. Clearing
    /// falls back to the derived repo/cwd name — see [`Workspace::display_name`].
    pub fn rename(cx: &mut gpui::App, id: WorkspaceId, name: Option<String>) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        if let Some(workspace) = store.workspaces.get_mut(id) {
            workspace.name = name.clone();
        }
        store.workspaces.save();
        // The name is the machine's fact now — its tree is what other clients
        // list this workspace from.
        crate::ui::tree_sync::fire_workspace_op(cx, id, move |ws| {
            tty7_core::daemon::control::ControlRequest::WorkspaceRename {
                workspace: ws,
                name,
            }
        });
    }

    /// Pick the one workspace launch will show, and detach every other one that
    /// was still open at the last quit.
    ///
    /// The detaching is the point: `open` means "a window is showing this", and
    /// launch is about to make that false for all but one of them. Leaving the
    /// rest marked open would have the switcher badge them "open" with no window
    /// to switch to, and would have the *next* quit believe they were on screen.
    /// Their panes are untouched — this is exactly the state
    /// [`close_window`](Self::close_window) leaves behind, reached in bulk.
    ///
    /// Returns `None` when nothing was open, which launch reads as "come up on
    /// the home page".
    pub fn restore_one(cx: &mut gpui::App) -> Option<WorkspaceId> {
        let store = Self::try_store(cx)?;
        let keep = store.workspaces.workspace_to_restore()?;
        let mut detached = 0usize;
        for workspace in &mut store.workspaces.workspaces {
            if workspace.open && workspace.id != keep {
                workspace.open = false;
                detached += 1;
            }
        }
        store.workspaces.active = Some(keep);
        store.workspaces.save();
        if detached > 0 {
            log::info!("launch: restoring 1 workspace, left {detached} detached");
        }
        Some(keep)
    }

    /// Detach a workspace: its window is gone, but the panes keep running in
    /// the daemon and the entry stays for the picker to reopen.
    pub fn close_window(cx: &mut gpui::App, id: WorkspaceId) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        if let Some(workspace) = store.workspaces.get_mut(id) {
            workspace.open = false;
            workspace.touch();
        }
        store.workspaces.save();
    }

    /// Drop the pane ids a workspace claims, keeping its layout. Answers
    /// whether anything changed, so a caller can skip the follow-up push to a
    /// remote that owns the record.
    ///
    /// Called right after those panes have been killed — see
    /// [`Workspace::forget_pane_ids`] for why the ids have to go rather than
    /// being left for the reattach to trip over.
    pub fn forget_pane_ids(cx: &mut gpui::App, id: WorkspaceId) -> bool {
        let Some(store) = Self::try_store(cx) else {
            return false;
        };
        let Some(workspace) = store.workspaces.get_mut(id) else {
            return false;
        };
        let forgotten = workspace.forget_pane_ids();
        if forgotten == 0 {
            return false;
        }
        store.workspaces.save();
        log::info!("workspace {id} forgot {forgotten} pane id(s): its sessions were ended");
        true
    }

    /// Forget a workspace entirely — the explicit "Close Workspace" action.
    /// The caller is responsible for killing its daemon panes first; this only
    /// drops the bookkeeping.
    pub fn remove(cx: &mut gpui::App, id: WorkspaceId) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        store.workspaces.workspaces.retain(|w| w.id != id);
        if store.workspaces.active == Some(id) {
            store.workspaces.active = None;
        }
        store.workspaces.save();
    }

    // ----- the client / remote storage split -------------------

    /// The machine a workspace's panes are on. `HostId::LOCAL` for a workspace
    /// this client owns, and for an id that is no longer on file — a window
    /// whose workspace vanished is showing nothing, and "nothing" is here.
    pub fn host_of(cx: &gpui::App, id: WorkspaceId) -> HostId {
        host_for(Self::all(cx), id)
    }

    /// The remote a workspace points at, or `None` when it is a local one.
    pub fn remote_ref(cx: &gpui::App, id: WorkspaceId) -> Option<RemoteRef> {
        Self::all(cx).get(id).and_then(|w| w.host.clone())
    }

    /// Whether this client can reach the machine `id`'s panes are on *right
    /// now* — the predicate both halves of the layout cache turn on
    /// ([`claimable_session`], [`record_session`]).
    ///
    /// A local workspace is always reachable: its daemon is this machine's, and
    /// a gate that could answer otherwise for a local window would stop it
    /// saving its own tabs.
    pub fn machine_is_connected(cx: &mut gpui::App, id: WorkspaceId) -> bool {
        let Some(host) = Self::remote_ref(cx, id) else {
            return true;
        };
        crate::ui::remote_connect::RemoteConnections::get(cx, host.host_id()).is_some()
    }

    /// The client-side entry for `host` — the existing one if this machine has
    /// seen that workspace before, a fresh one otherwise.
    ///
    /// The two ids are deliberately different things: the entry has its own
    /// [`WorkspaceId`] (this client's handle, what the window registry and the
    /// Window menu key on), and `host.workspace` is the id **on the remote**,
    /// which is what the `WorkspacePut` / `WorkspaceGet` calls carry. Reusing
    /// one id for both would collide the moment two machines minted the same
    /// uuid, and would quietly make a client id meaningful off this machine.
    ///
    /// The entry is matched on the whole [`RemoteRef`], so the same workspace id
    /// on two different machines is two entries, and reconnecting to one you
    /// have opened before reuses its window geometry rather than cascading a new
    /// window every time.
    pub fn claim_remote(cx: &mut gpui::App, host: RemoteRef) -> WorkspaceId {
        let Some(store) = Self::try_store(cx) else {
            return WorkspaceId::new();
        };
        let existing = store
            .workspaces
            .workspaces
            .iter()
            .find(|w| w.host.as_ref() == Some(&host))
            .map(|w| w.id);
        let id = match existing {
            Some(id) => id,
            None => {
                let workspace = Workspace::on_remote(host);
                let id = workspace.id;
                store.workspaces.workspaces.push(workspace);
                id
            }
        };
        store.workspaces.save();
        id
    }
}

/// The machine a window showing `id` is bound to.
///
/// The whole of "one window, one machine" reduces to this being a *function*: a
/// window shows one workspace, a workspace names one host, so a window has one
/// host and there is no arrangement of the data in which it has two. Split out
/// from [`WorkspaceStore::host_of`] so it can be tested against a workspace set
/// built by hand, with no globals and nothing written to disk.
///
/// An id that is not on file answers `LOCAL`: a window whose workspace was
/// deleted out from under it is showing nothing, and "nothing" is here — the
/// safe answer, because it is the one that refuses no local action.
pub(crate) fn host_for(workspaces: &Workspaces, id: WorkspaceId) -> HostId {
    workspaces
        .get(id)
        .map(|w| w.host_id())
        .unwrap_or(HostId::LOCAL)
}

/// Whether rebinding a window from `previous` to `current` moved it to another
/// machine — the moment every piece of per-*window* state that outlived the
/// swap has to be reconsidered.
pub(crate) fn crosses_machines(previous: HostId, current: HostId) -> bool {
    previous != current
}

/// The layout a window opening on `workspace` may rebuild — the read-side twin
/// of [`record_session`], and by now only the *fallback* source: the machine's
/// tree is the layout authority, and the hydration that follows a claim is
/// what actually fills the window. What this still decides is the shape the
/// window opens in.
///
/// A remote workspace opens empty unconditionally — its layout lives on
/// another machine, and this client's cached copy is an import fallback, not
/// something to build panes from. A local one hands back the cached layout for
/// the paths that deliberately skip hydration (restore off, and the hydration
/// import itself).
fn claimable_session(workspace: &mut Workspace, reachable: bool) -> Session {
    let _ = reachable;
    if workspace.is_remote() {
        return Session::default();
    }
    workspace.session.clone()
}

/// Write a window's layout onto its entry — the write-side twin of
/// [`claimable_session`].
///
/// A window that cannot reach its machine is not describing that machine's
/// layout (its panes failed to restore, or are sitting there disconnected), so
/// it records nothing rather than replacing the copy we have with the
/// wreckage. The machine's tree is the authority; this entry is the cache the
/// hydration import falls back to.
fn record_session(workspace: &mut Workspace, session: Session, reachable: bool) {
    if workspace.is_remote() && !reachable {
        return;
    }
    workspace.session = session;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(cwd: &str) -> SessionPane {
        SessionPane::Leaf {
            cwd: Some(std::path::PathBuf::from(cwd)),
            pane_id: Some(7),
            ssh_spec: None,
            agent: None,
            agent_session_id: None,
            agent_launch_argv: None,
        }
    }

    fn local_layout() -> Session {
        Session {
            tabs: vec![SessionTab {
                name: None,
                tree_id: None,
                sidebar_group: None,
                pane: leaf("/Users/me/work"),
            }],
            ..Session::default()
        }
    }

    fn remote_ref() -> RemoteRef {
        RemoteRef::new(
            RemoteTarget::Alias {
                alias: "build-box".into(),
            },
            WorkspaceId::new(),
        )
    }

    /// A local workspace records its layout the way it always did.
    #[test]
    fn a_local_workspace_stores_its_own_layout() {
        let mut workspace = Workspace::default();
        record_session(&mut workspace, local_layout(), true);
        assert_eq!(workspace.session.tabs.len(), 1);
        assert_eq!(workspace.pane_ids(), vec![7]);
    }

    /// The point of the whole cache: a connected remote window's layout is
    /// kept, so the next launch has something to open from and
    /// `remote_payload` has something to push. Without this, reconnecting to a
    /// machine gives an empty window every time.
    #[test]
    fn a_connected_remote_workspace_stores_its_layout() {
        let mut workspace = Workspace::on_remote(remote_ref());
        record_session(&mut workspace, local_layout(), true);
        assert_eq!(workspace.session.tabs.len(), 1);
        assert_eq!(
            workspace.pane_ids(),
            vec![7],
            "the pane ids are the remote daemon's, and are what a reconnect re-attaches"
        );
    }

    /// A local workspace opens on the layout it saved.
    #[test]
    fn a_local_workspace_reopens_its_saved_layout() {
        let mut workspace = Workspace {
            session: local_layout(),
            ..Workspace::default()
        };
        let claimed = claimable_session(&mut workspace, true);
        assert_eq!(claimed.tabs.len(), 1);
        // And the entry is left alone.
        assert_eq!(workspace.session.tabs.len(), 1);
    }

    /// A remote workspace opens empty even when its machine is connected: the
    /// machine's tree is the layout authority now, and the hydration that
    /// follows the claim fills the window from it. The cached copy stays —
    /// it is the import fallback, not the source.
    #[test]
    fn a_connected_remote_workspace_still_opens_empty_for_the_tree_to_fill() {
        let mut workspace = Workspace::on_remote(remote_ref());
        workspace.session = local_layout();
        let claimed = claimable_session(&mut workspace, true);
        assert!(claimed.tabs.is_empty());
        assert_eq!(workspace.session.tabs.len(), 1, "the cache is kept");
    }

    /// With the machine unreachable, `List` answers nothing, so every leaf
    /// would miss its live pane and try to spawn a fresh one beside it. The
    /// window opens empty instead — and, the half that took a real launch to
    /// get right, **the cached layout survives**: it is what the connect path
    /// rebuilds the window from a moment later.
    #[test]
    fn an_unreachable_remote_workspace_opens_empty_but_keeps_its_layout() {
        let mut workspace = Workspace::on_remote(remote_ref());
        workspace.session = local_layout();

        let claimed = claimable_session(&mut workspace, false);
        assert!(claimed.tabs.is_empty(), "the window must open with no tabs");
        assert_eq!(
            workspace.session.tabs.len(),
            1,
            "and the layout must still be there for the connect to rebuild from"
        );
    }

    /// The write-side twin: a window that could not restore its panes is not
    /// describing the machine's layout, so its empty tab list must not replace
    /// the copy we have of it.
    #[test]
    fn an_unreachable_remote_window_does_not_overwrite_the_cached_layout() {
        let mut workspace = Workspace::on_remote(remote_ref());
        workspace.session = local_layout();
        record_session(&mut workspace, Session::default(), false);
        assert_eq!(workspace.session.tabs.len(), 1);
    }

    /// The launch path end to end, with the store's own reachability lookup
    /// rather than a hand-passed flag: nothing has ever connected to that
    /// machine in this process, so the window opens empty and the layout it
    /// will be rebuilt from is still on file afterwards.
    ///
    /// The config dir is pinned first because `claim` and `record` both persist
    /// — without it this test would rewrite the developer's real
    /// `session.json`.
    #[gpui::test]
    fn an_unconnected_machine_keeps_its_workspace_layout_across_a_claim(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            // The same path every other config-pinning test in this process
            // uses: `set_config_dir` is first-call-wins, so a test that pinned
            // a *different* scratch would silently redirect whichever tests
            // lost the race away from the directory they then read back.
            crate::core::config::pin_test_config_dir();

            let mut entry = Workspace::on_remote(remote_ref());
            entry.session = local_layout();
            let id = entry.id;
            WorkspaceStore::install_for_test(
                cx,
                Workspaces {
                    workspaces: vec![entry],
                    active: None,
                },
            );

            let (claimed, session) = WorkspaceStore::claim(cx, Some(id));
            assert_eq!(claimed, id);
            assert!(
                session.tabs.is_empty(),
                "an unreachable machine's window opens empty"
            );

            // …and the window recording that emptiness does not erase what the
            // machine still has.
            WorkspaceStore::record(cx, id, Session::default(), None);
            assert_eq!(
                WorkspaceStore::all(cx).get(id).unwrap().session.tabs.len(),
                1,
                "the cached layout must survive for the connect to rebuild from"
            );
        });
    }

    /// "End Sessions" kills the panes and then has to say so on file, or
    /// reopening the workspace walks into the reattach path with ids nothing
    /// answers to. The second call answering `false` is what lets the caller
    /// skip the push that follows.
    #[gpui::test]
    fn forgetting_a_workspaces_panes_is_recorded_once(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::core::config::pin_test_config_dir();

            let mut entry = Workspace::on_remote(remote_ref());
            entry.session = local_layout();
            let id = entry.id;
            WorkspaceStore::install_for_test(
                cx,
                Workspaces {
                    workspaces: vec![entry],
                    active: None,
                },
            );
            assert_eq!(WorkspaceStore::all(cx).get(id).unwrap().pane_ids(), vec![7]);

            assert!(WorkspaceStore::forget_pane_ids(cx, id));
            let after = WorkspaceStore::all(cx).get(id).unwrap();
            assert!(after.pane_ids().is_empty());
            assert_eq!(
                after.session.tabs.len(),
                1,
                "the layout is exactly what reopening rebuilds from"
            );
            assert!(
                !WorkspaceStore::forget_pane_ids(cx, id),
                "nothing left to forget"
            );
        });
    }

    /// **The window/host invariant, as a test.**
    ///
    /// A window is one machine. The inverse is listed under
    /// *never do this*, and the M5 data layer spends that guarantee — a
    /// workspace stores `host` once instead of per pane, and `sidebar_group`
    /// stays a bare `PathBuf` — so it has to be nailed down rather than
    /// believed.
    ///
    /// What is actually being asserted: for any workspace set containing local
    /// and remote entries on several machines, the host a window binds to is a
    /// *function* of the workspace it shows. Every id answers exactly one
    /// machine, and no id answers two.
    #[test]
    fn a_window_binds_to_exactly_one_machine() {
        let build = RemoteTarget::Alias {
            alias: "build-box".into(),
        };
        let gpu = RemoteTarget::direct("me", "gpu.lab", 2222);

        let local = Workspace::default();
        let build_a = Workspace::on_remote(RemoteRef::new(build.clone(), WorkspaceId::new()));
        let build_b = Workspace::on_remote(RemoteRef::new(build, WorkspaceId::new()));
        let gpu_a = Workspace::on_remote(RemoteRef::new(gpu, WorkspaceId::new()));
        let (local_id, build_a_id, build_b_id, gpu_id) =
            (local.id, build_a.id, build_b.id, gpu_a.id);

        let workspaces = Workspaces {
            workspaces: vec![local, build_a, build_b, gpu_a],
            ..Workspaces::default()
        };

        // Three machines are represented, and they stay apart.
        let l = host_for(&workspaces, local_id);
        let b1 = host_for(&workspaces, build_a_id);
        let b2 = host_for(&workspaces, build_b_id);
        let g = host_for(&workspaces, gpu_id);
        assert_eq!(l, HostId::LOCAL);
        assert_eq!(b1, b2, "two workspaces on one box share its connection");
        assert_ne!(b1, g);
        assert_ne!(b1, l);
        assert_ne!(g, l);

        // The answer is stable: asking twice cannot give a window a second host.
        assert_eq!(host_for(&workspaces, build_a_id), b1);

        // And a window whose workspace was deleted underneath it falls back to
        // local rather than to some other machine's id.
        assert_eq!(host_for(&workspaces, WorkspaceId::new()), HostId::LOCAL);

        // Only a host change is a machine change — the trigger for dropping the
        // per-window state (the closed-tab stack) that could otherwise carry a
        // tab across.
        assert!(!crosses_machines(b1, b2));
        assert!(crosses_machines(l, b1));
        assert!(crosses_machines(b1, g));
    }

    /// Two workspaces on one machine answer one `HostId`; a workspace on another
    /// machine answers a different one. That equality is what every "is this the
    /// same machine?" check in the window layer is built on.
    #[test]
    fn host_ids_group_by_machine_not_by_workspace() {
        let build = RemoteTarget::Alias {
            alias: "build-box".into(),
        };
        let other = RemoteTarget::Alias {
            alias: "other-box".into(),
        };
        let a = Workspace::on_remote(RemoteRef::new(build.clone(), WorkspaceId::new()));
        let b = Workspace::on_remote(RemoteRef::new(build, WorkspaceId::new()));
        let c = Workspace::on_remote(RemoteRef::new(other, WorkspaceId::new()));
        let local = Workspace::default();

        assert_eq!(a.host_id(), b.host_id());
        assert_ne!(a.host_id(), c.host_id());
        assert_eq!(local.host_id(), HostId::LOCAL);
        assert_ne!(a.host_id(), HostId::LOCAL);
    }
}

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
        let mut workspaces = Workspaces::load().unwrap_or_default();
        let dropped = workspaces.dedupe_pane_ids();
        if dropped > 0 {
            log::warn!(
                "session.json claimed {dropped} pane(s) from more than one workspace; \
                 the stale claims will spawn fresh shells instead"
            );
        }
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
        let instance = Self::serving_instance(cx, id);
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
        let claimed = (
            workspace.id,
            claimable_session(workspace, reachable, instance.as_deref()),
        );
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
        let instance = Self::serving_instance(cx, Some(id));
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        let Some(workspace) = store.workspaces.get_mut(id) else {
            // The workspace was closed out from under us (its window is
            // tearing down); nothing to record.
            return;
        };
        record_session(workspace, session, reachable, instance);
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
    }

    /// Set (or clear, with `None`) a workspace's user-chosen name. Clearing
    /// falls back to the derived repo/cwd name — see [`Workspace::display_name`].
    pub fn rename(cx: &mut gpui::App, id: WorkspaceId, name: Option<String>) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        if let Some(workspace) = store.workspaces.get_mut(id) {
            workspace.name = name;
        }
        store.workspaces.save();
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

    /// The process whose pane ids this workspace's record is about: this
    /// machine's daemon for a local workspace, the far machine's `tty7-server`
    /// for a remote one. `None` when it cannot be named — an older peer, a
    /// machine not connected right now, or a brand-new workspace with no host
    /// yet — which every reader treats as "no instance check possible".
    ///
    /// One function for both because [`Workspace::daemon_instance`] means the
    /// same thing on both sides. It used to be local-only, on the reasoning
    /// that a remote server's identity is tracked live per connection instead
    /// — but that live map lives in memory, so it is empty on the launch that
    /// matters most: the one where the client was closed while the remote
    /// server was replaced.
    pub fn serving_instance(cx: &mut gpui::App, id: Option<WorkspaceId>) -> Option<String> {
        match id.and_then(|id| Self::remote_ref(cx, id)) {
            Some(host) => crate::ui::remote_connect::RemoteConnections::get(cx, host.host_id())
                .map(|h| h.peer().instance.clone())
                .filter(|instance| !instance.is_empty()),
            None => crate::daemon::spawn::local_daemon_instance(),
        }
    }

    /// Blank `id`'s saved pane ids when they were recorded against a different
    /// server process than `instance`, and persist that. Answers whether any
    /// were dropped.
    ///
    /// The remote counterpart of the check [`claimable_session`] runs for a
    /// local workspace at claim time. It cannot run there for a remote one: at
    /// claim time the machine is usually not connected yet, so there is no
    /// instance to compare against. The reconnect is the first moment the
    /// answer exists, which is where this is called from.
    pub fn forget_stale_pane_ids(cx: &mut gpui::App, id: WorkspaceId, instance: &str) -> bool {
        let current = (!instance.is_empty()).then_some(instance);
        let Some(store) = Self::try_store(cx) else {
            return false;
        };
        let Some(workspace) = store.workspaces.get_mut(id) else {
            return false;
        };
        let dropped = workspace.forget_stale_pane_ids(current);
        if dropped == 0 {
            return false;
        }
        // Stamped now rather than left for the next save: the record has just
        // been made to describe *this* server, and a crash before the window
        // saves must not leave it claiming the old process again.
        workspace.daemon_instance = current.map(str::to_string);
        store.workspaces.save();
        log::info!(
            "workspace {id}: {dropped} saved pane id(s) belong to a previous \
             tty7-server process; rebuilding from the layout"
        );
        true
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

    /// Merge an authoritative record pulled from the remote into the client's
    /// entry. Only the remote-owned fields move; `open`, `window` and `host`
    /// stay as this machine left them (see [`Workspace::apply_remote_json`]).
    ///
    /// A record that will not decode is dropped with a log line rather than
    /// failing the open: the layout is recoverable on the next push, an
    /// unopenable workspace is not.
    pub fn apply_remote(cx: &mut gpui::App, id: WorkspaceId, record: &serde_json::Value) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        let Some(workspace) = store.workspaces.get_mut(id) else {
            return;
        };
        if let Err(e) = workspace.apply_remote_json(record) {
            log::warn!("remote workspace {id} sent a record this build cannot read: {e}");
            return;
        }
        store.workspaces.save();
    }

    /// What to send the remote for `id`: its store key and the remote-owned half
    /// of the record. `None` for a local workspace — there is nobody to send to.
    pub fn remote_payload(
        cx: &gpui::App,
        id: WorkspaceId,
    ) -> Option<(RemoteRef, String, serde_json::Value)> {
        let workspace = Self::all(cx).get(id)?;
        let host = workspace.host.clone()?;
        let key = host.store_key();
        // The record travels under the *remote's* id, not the client entry's:
        // the remote store is keyed by its own ids, and a record whose `id`
        // disagreed with its key would be a workspace that renames itself on
        // every round trip.
        let mut record = workspace.to_remote_json();
        if let Some(obj) = record.as_object_mut() {
            obj.insert("id".to_string(), serde_json::json!(key));
        }
        Some((host, key, record))
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
/// of [`record_session`].
///
/// A remote entry's `session` is this client's copy of a record the machine
/// owns: pulled on connect, refreshed on every `WorkspaceChanged`, pushed back
/// on every structural change. Rebuilding from it is the whole of "reconnecting
/// gets my tabs back", and it is safe to do because every pane it names is
/// routed by [`crate::ui::remote_workspace::pane_workspace_for`] — a leaf in a
/// remote workspace attaches or spawns *over there*, and a machine that cannot
/// be reached fails the spawn rather than falling back to a local shell.
///
/// `reachable` is what keeps that guarantee from being theoretical. With the
/// link down, `List` answers nothing, so every leaf would miss its live pane and
/// try to spawn a fresh one — either failing (an empty window, having thrown the
/// layout away) or, worse, landing a second shell next to the one still running
/// over there. So an unreachable remote workspace opens empty **without
/// touching the cached layout**, and
/// [`crate::ui::remote_workspace`]'s connect path rebuilds the window the moment
/// the machine answers.
/// `current_instance` is the identity of the process serving this workspace's
/// panes (see [`WorkspaceStore::serving_instance`]). A workspace whose saved ids
/// were recorded against a different one blanks them first — after a restart the
/// numbers begin again at 1, so a stale id would otherwise pass the aliveness
/// check by landing on whatever unrelated pane holds it now. Blanked in the
/// stored entry too, not just the returned copy, so the record stops claiming
/// panes that no longer exist even if the window never saves again.
///
/// A remote workspace usually reaches the early return above instead: at claim
/// time its machine is not connected yet, so there is no instance to compare and
/// no layout to hand back. `remote_workspace::finish_attempt` runs the same
/// check the moment the connect answers, which is the first point it can.
fn claimable_session(
    workspace: &mut Workspace,
    reachable: bool,
    current_instance: Option<&str>,
) -> Session {
    if workspace.is_remote() && !reachable {
        return Session::default();
    }
    let dropped = workspace.forget_stale_pane_ids(current_instance);
    if dropped > 0 {
        log::info!(
            "workspace {}: {dropped} saved pane id(s) belong to a previous serving \
             process; restoring with fresh shells (and agent resume where recorded)",
            workspace.id
        );
    }
    workspace.session.clone()
}

/// Write a window's layout onto its entry — the write-side twin of
/// [`claimable_session`].
///
/// A window that cannot reach its machine is not describing that machine's
/// layout (its panes failed to restore, or are sitting there disconnected), so
/// it records nothing rather than replacing the copy we have with the wreckage.
/// The remote's own `workspaces.json` is still the authority; this entry is the
/// cache the next launch opens from.
/// The record is stamped with the process its pane ids came from (`instance`):
/// this machine's daemon for a local workspace, the far machine's
/// `tty7-server` for a remote one. That is what lets the next launch tell a
/// surviving process from a replaced one — see [`claimable_session`] and
/// [`WorkspaceStore::forget_stale_pane_ids`].
///
/// The unreachable early return doubles as the guard on that stamp: with the
/// machine down there is no instance to record, and writing `None` over a good
/// one would throw away the very comparison the next connect needs.
fn record_session(
    workspace: &mut Workspace,
    session: Session,
    reachable: bool,
    instance: Option<String>,
) {
    if workspace.is_remote() && !reachable {
        return;
    }
    workspace.session = session;
    workspace.daemon_instance = instance;
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
        record_session(&mut workspace, local_layout(), true, None);
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
        record_session(&mut workspace, local_layout(), true, None);
        assert_eq!(workspace.session.tabs.len(), 1);
        assert_eq!(
            workspace.pane_ids(),
            vec![7],
            "the pane ids are the remote daemon's, and are what a reconnect re-attaches"
        );
    }

    /// A remote workspace's record is stamped with the **server's** instance,
    /// not left blank. That stamp is the only part of "which process minted
    /// these ids" that survives the client being closed, and it is what the
    /// next connect compares against before re-attaching anything.
    #[test]
    fn a_connected_remote_workspace_records_the_serving_instance() {
        let mut workspace = Workspace::on_remote(remote_ref());
        record_session(
            &mut workspace,
            local_layout(),
            true,
            Some("server-a".to_string()),
        );
        assert_eq!(workspace.daemon_instance.as_deref(), Some("server-a"));
    }

    /// …and an unreachable machine does not un-stamp it. `None` there means
    /// "nobody to ask", and writing it over a good value would disarm the very
    /// check the next connect needs — the ids would look current again.
    #[test]
    fn an_unreachable_remote_window_does_not_erase_the_recorded_instance() {
        let mut workspace = Workspace::on_remote(remote_ref());
        workspace.session = local_layout();
        workspace.daemon_instance = Some("server-a".to_string());
        record_session(&mut workspace, Session::default(), false, None);
        assert_eq!(workspace.daemon_instance.as_deref(), Some("server-a"));
        assert_eq!(workspace.session.tabs.len(), 1, "and the layout stays too");
    }

    /// A local workspace opens on the layout it saved.
    #[test]
    fn a_local_workspace_reopens_its_saved_layout() {
        let mut workspace = Workspace {
            session: local_layout(),
            ..Workspace::default()
        };
        let claimed = claimable_session(&mut workspace, true, None);
        assert_eq!(claimed.tabs.len(), 1);
        // And the entry is left alone.
        assert_eq!(workspace.session.tabs.len(), 1);
    }

    /// Claiming a local workspace whose ids were recorded against a *different*
    /// daemon process blanks them — in the returned session **and** in the
    /// stored entry. After a reboot the numbers restart from 1, so a stale id
    /// passes the aliveness check by landing on whatever unrelated pane holds
    /// it now; blanking is what turns that into an honest fresh spawn (with
    /// the agent resume the leaf recorded).
    #[test]
    fn claiming_a_local_workspace_from_another_daemon_process_blanks_its_ids() {
        let mut workspace = Workspace {
            session: local_layout(),
            daemon_instance: Some("previous-boot".into()),
            ..Workspace::default()
        };
        let leaf_id = |session: &Session| match &session.tabs[0].pane {
            SessionPane::Leaf { pane_id, .. } => *pane_id,
            SessionPane::Split { .. } => panic!("the fixture is a single leaf"),
        };
        let claimed = claimable_session(&mut workspace, true, Some("current-boot"));
        assert_eq!(claimed.tabs.len(), 1, "the layout still restores");
        assert_eq!(
            leaf_id(&claimed),
            None,
            "but no leaf may attach by a number from a dead daemon"
        );
        assert!(workspace.pane_ids().is_empty(), "the entry agrees");

        // Same process → the ids stay attachable.
        let mut workspace = Workspace {
            session: local_layout(),
            daemon_instance: Some("current-boot".into()),
            ..Workspace::default()
        };
        let claimed = claimable_session(&mut workspace, true, Some("current-boot"));
        assert_eq!(leaf_id(&claimed), Some(7));
    }

    /// A connected remote workspace reopens on the layout its machine last
    /// reported — the read half of "reconnecting gets my tabs back".
    #[test]
    fn a_connected_remote_workspace_reopens_its_layout() {
        let mut workspace = Workspace::on_remote(remote_ref());
        workspace.session = local_layout();
        let claimed = claimable_session(&mut workspace, true, None);
        assert_eq!(claimed.tabs.len(), 1);
        assert_eq!(workspace.session.tabs.len(), 1);
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

        let claimed = claimable_session(&mut workspace, false, None);
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
        record_session(&mut workspace, Session::default(), false, None);
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

    /// **The cold-launch half of the restart check.** `RemoteLinks::instances`
    /// is in memory, so on the first connect after the client starts every
    /// machine is a first sighting and nothing is judged a restart. A server
    /// replaced while the client was closed would therefore sail through, and
    /// its recycled ids — daemons number panes from 1 — would attach to
    /// whatever unrelated shells hold those numbers now. The stamp on the
    /// record is what closes that, so this is the test that has to hold.
    #[gpui::test]
    fn a_remote_workspace_drops_pane_ids_minted_by_a_previous_server(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            crate::core::config::pin_test_config_dir();

            let mut entry = Workspace::on_remote(remote_ref());
            entry.session = local_layout();
            entry.daemon_instance = Some("server-a".to_string());
            let id = entry.id;
            WorkspaceStore::install_for_test(
                cx,
                Workspaces {
                    workspaces: vec![entry],
                    active: None,
                },
            );

            // Same process: these ids still name the panes they always did.
            assert!(!WorkspaceStore::forget_stale_pane_ids(cx, id, "server-a"));
            assert_eq!(WorkspaceStore::all(cx).get(id).unwrap().pane_ids(), vec![7]);

            // An unknown instance is never judged — a peer too old to report
            // one must not cost the user every pane on the machine.
            assert!(!WorkspaceStore::forget_stale_pane_ids(cx, id, ""));
            assert_eq!(WorkspaceStore::all(cx).get(id).unwrap().pane_ids(), vec![7]);

            // Replaced: the claims go, the layout stays, and the stamp moves on.
            assert!(WorkspaceStore::forget_stale_pane_ids(cx, id, "server-b"));
            let after = WorkspaceStore::all(cx).get(id).unwrap();
            assert!(after.pane_ids().is_empty());
            assert_eq!(
                after.session.tabs.len(),
                1,
                "the layout is exactly what the rebuild draws from"
            );
            assert_eq!(
                after.daemon_instance.as_deref(),
                Some("server-b"),
                "stamped now, so a crash before the next save cannot re-arm the old claim"
            );

            // And the same server is not a restart twice over.
            assert!(!WorkspaceStore::forget_stale_pane_ids(cx, id, "server-b"));
        });
    }

    /// **Why clearing the ids locally is not enough.** The remote owns the
    /// record, so reopening pulls its copy over the client's — and
    /// a copy that still claims the killed panes puts them straight back. This
    /// is the constraint `windows::forget_killed_panes` pushes to satisfy; if
    /// this assertion ever flips, that push is dead weight.
    #[test]
    fn a_remote_record_reinstates_pane_ids_a_client_only_clear_dropped() {
        let mut theirs = Workspace::on_remote(remote_ref());
        theirs.session = local_layout();
        let record = theirs.to_remote_json();

        let mut ours = Workspace::on_remote(remote_ref());
        ours.session = local_layout();
        ours.forget_pane_ids();
        assert!(ours.pane_ids().is_empty());

        ours.apply_remote_json(&record).unwrap();
        assert_eq!(
            ours.pane_ids(),
            vec![7],
            "the machine's copy wins, so the clear has to reach it"
        );
    }

    /// The remote-bound payload travels under the *remote's* id, so a record
    /// pushed and pulled back names the same workspace both times.
    #[test]
    fn the_remote_payload_is_keyed_by_the_remote_id_not_the_client_entry() {
        let host = remote_ref();
        let workspace = Workspace::on_remote(host.clone());
        let mut record = workspace.to_remote_json();
        record
            .as_object_mut()
            .unwrap()
            .insert("id".into(), serde_json::json!(host.store_key()));
        assert_eq!(host.store_key(), host.workspace.to_string());
        assert_ne!(host.store_key(), workspace.id.to_string());
        assert_eq!(record["id"], serde_json::json!(host.store_key()));
        // The client-owned half never crosses.
        for client_only in tty7_core::core::session::CLIENT_OWNED_FIELDS {
            assert!(
                record.get(*client_only).is_none(),
                "{client_only} must not be sent to the remote"
            );
        }
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

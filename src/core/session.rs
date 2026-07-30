//! The gpui-facing half of view-state persistence.
//!
//! The on-disk model — [`WindowView`], [`WindowViews`] and the `views.json`
//! IO — lives in `tty7-core` beside the in-memory [`Session`] shapes. What is
//! left here is [`WorkspaceStore`], which is a gpui `Global` and threads every
//! mutation through `&mut App`.
//!
//! The store holds **no layout**. A workspace's tabs and panes live in its
//! machine's daemon-owned tree; this file remembers only what that tree cannot
//! — which workspaces this client knows, which machine each is on, window
//! geometry, the open flag, and focus recency.

pub use tty7_core::core::session::{
    RemoteRef, RemoteTarget, Session, SessionAxis, SessionPane, SessionTab, WindowView,
    WindowViews, WorkspaceId,
};
pub use tty7_core::host::HostId;

/// App-level owner of `views.json`, and the single writer to it.
///
/// Windows never touch the file themselves. Each one pushes *its* view state
/// in and the store persists the merged whole — without that, two windows
/// doing read-modify-write on the shared file would have the last writer
/// clobber the other's entries. It also means a window that is closing can
/// record its final state after its own entity is already being torn down.
pub struct WorkspaceStore {
    views: WindowViews,
}

impl gpui::Global for WorkspaceStore {}

impl WorkspaceStore {
    /// Read `views.json` and install the result as the app global. Call once,
    /// before the first window is built.
    pub fn init(cx: &mut gpui::App) {
        let views = WindowViews::load().unwrap_or_default();
        cx.set_global(Self { views });
    }

    /// Install a store holding exactly `views`.
    ///
    /// Tests only, and it exists because [`init`](Self::init) reads the
    /// developer's real `views.json`: a test that needs a workspace to be on
    /// file must neither depend on what happens to be there nor risk writing to
    /// it. Every mutating helper already no-ops without the global, so this is
    /// the one thing a test cannot do for itself.
    #[cfg(test)]
    pub fn install_for_test(cx: &mut gpui::App, views: WindowViews) {
        cx.set_global(Self { views });
    }

    /// Every known workspace. Read-only — mutations go through the helpers so
    /// the file stays in step.
    ///
    /// Reads as empty when the store was never installed. That is the headless
    /// test harness, which builds windows directly rather than through
    /// `ui::windows::open`; "no saved workspaces" is the correct reading there,
    /// and it keeps a missing global from panicking a render.
    pub fn all(cx: &gpui::App) -> &WindowViews {
        static EMPTY: std::sync::OnceLock<WindowViews> = std::sync::OnceLock::new();
        match cx.try_global::<Self>() {
            Some(store) => &store.views,
            None => EMPTY.get_or_init(WindowViews::default),
        }
    }

    /// The store, or `None` when it was never installed (tests). Every mutating
    /// helper goes through this so a headless window is a no-op rather than a
    /// panic — and, importantly, so tests never write to a real `views.json`.
    fn try_store(cx: &mut gpui::App) -> Option<&mut Self> {
        cx.has_global::<Self>().then(|| cx.global_mut::<Self>())
    }

    /// Take over an existing workspace to show in a window, or mint a fresh one
    /// when `id` is `None` / no longer on file (the "New Workspace" path).
    /// Marks it open and returns its id. The layout is not this store's to
    /// hand out — the window opens empty and the tree hydration fills it.
    pub fn claim(cx: &mut gpui::App, id: Option<WorkspaceId>) -> WorkspaceId {
        let Some(store) = Self::try_store(cx) else {
            // No store (tests): hand back a detached identity so the window
            // still builds, but nothing is persisted.
            return WorkspaceId::new();
        };
        let id = id.filter(|id| store.views.get(*id).is_some());
        let view = match id {
            Some(id) => store.views.get_mut(id).expect("filtered above"),
            None => {
                store.views.views.push(WindowView::default());
                store.views.views.last_mut().expect("just pushed")
            }
        };
        view.open = true;
        view.touch();
        let claimed = view.id;
        store.views.active = Some(claimed);
        store.views.save();
        claimed
    }

    /// Record a window's geometry and persist. Called on every structural
    /// change (the same funnel the tree sync rides), so reopening the
    /// workspace lands where the user left it.
    ///
    /// The display hint rides along for the same reason the geometry does: it is
    /// what the picker needs about a workspace whose machine is *not* answering,
    /// and the moment to capture it is while it still is. Read before the store
    /// is borrowed — the answer comes from another global.
    pub fn record_geometry(
        cx: &mut gpui::App,
        id: WorkspaceId,
        window: crate::core::window_state::WindowState,
    ) {
        let hint = Self::all(cx)
            .get(id)
            .and_then(|view| crate::ui::machine_mirror::display_hint(cx, view));
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        let Some(view) = store.views.get_mut(id) else {
            // The workspace was closed out from under us (its window is
            // tearing down); nothing to record.
            return;
        };
        view.window = Some(window);
        // Only ever replaced by something better: a machine that has gone quiet
        // must not blank the label it gave us while it was up.
        if let Some((label, subject)) = hint {
            view.label = Some(label);
            view.subject = subject;
        }
        store.views.save();
    }

    /// Mark the focused workspace, so the next launch restores focus to the
    /// window the user was actually in.
    pub fn focus(cx: &mut gpui::App, id: WorkspaceId) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        if let Some(view) = store.views.get_mut(id) {
            view.touch();
        }
        store.views.active = Some(id);
        store.views.save();
        // The machine's tree keeps its own recency (its pickers order by it),
        // so the focus is a fact to report there too.
        crate::ui::tree_sync::fire_workspace_op(cx, id, |ws| {
            tty7_core::daemon::control::ControlRequest::WorkspaceTouch { workspace: ws }
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
        let keep = store.views.workspace_to_restore()?;
        let mut detached = 0usize;
        for view in &mut store.views.views {
            if view.open && view.id != keep {
                view.open = false;
                detached += 1;
            }
        }
        store.views.active = Some(keep);
        store.views.save();
        if detached > 0 {
            log::info!("launch: restoring 1 workspace, left {detached} detached");
        }
        Some(keep)
    }

    /// Detach a workspace: its window is gone, but the panes keep running in
    /// the daemon and the entry stays for the picker to reopen.
    pub fn close_window(cx: &mut gpui::App, id: WorkspaceId) {
        // The last moment this client can see what the machine calls the
        // workspace — and a detached workspace is precisely what the picker
        // lists, so the hint matters most here. Read before the borrow, as in
        // [`record_geometry`](Self::record_geometry).
        let hint = Self::all(cx)
            .get(id)
            .and_then(|view| crate::ui::machine_mirror::display_hint(cx, view));
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        if let Some(view) = store.views.get_mut(id) {
            view.open = false;
            view.touch();
            if let Some((label, subject)) = hint {
                view.label = Some(label);
                view.subject = subject;
            }
        }
        store.views.save();
    }

    /// Forget a workspace entirely — the explicit "Close Workspace" action.
    /// The caller is responsible for the machine-side half (killing panes,
    /// `WorkspaceRemove`); this only drops the client's pointer.
    pub fn remove(cx: &mut gpui::App, id: WorkspaceId) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        store.views.views.retain(|w| w.id != id);
        if store.views.active == Some(id) {
            store.views.active = None;
        }
        store.views.save();
    }

    // ----- the client / machine split -------------------

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
    /// now*.
    ///
    /// A local workspace is always reachable: its daemon is this machine's, and
    /// a gate that could answer otherwise for a local window would stop it
    /// acting on its own workspace.
    pub fn machine_is_connected(cx: &mut gpui::App, id: WorkspaceId) -> bool {
        let Some(host) = Self::remote_ref(cx, id) else {
            return true;
        };
        crate::ui::remote_connect::HostLinks::get(cx, host.host_id()).is_some()
    }

    /// The client-side entry for `host` — the existing one if this machine has
    /// seen that workspace before, a fresh one otherwise.
    ///
    /// The two ids are deliberately different things: the entry has its own
    /// [`WorkspaceId`] (this client's handle, what the window registry and the
    /// Window menu key on), and `host.workspace` is the id **on the remote**,
    /// which is what the machine-tree operations carry. Reusing
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
            .views
            .views
            .iter()
            .find(|w| w.host.as_ref() == Some(&host))
            .map(|w| w.id);
        let id = match existing {
            Some(id) => id,
            None => {
                let view = WindowView::on_remote(host);
                let id = view.id;
                store.views.views.push(view);
                id
            }
        };
        store.views.save();
        id
    }
}

/// The machine a window showing `id` is bound to.
///
/// The whole of "one window, one machine" reduces to this being a *function*: a
/// window shows one workspace, a workspace names one host, so a window has one
/// host and there is no arrangement of the data in which it has two. Split out
/// from [`WorkspaceStore::host_of`] so it can be tested against a view set
/// built by hand, with no globals and nothing written to disk.
///
/// An id that is not on file answers `LOCAL`: a window whose workspace was
/// deleted out from under it is showing nothing, and "nothing" is here — the
/// safe answer, because it is the one that refuses no local action.
pub(crate) fn host_for(views: &WindowViews, id: WorkspaceId) -> HostId {
    views.get(id).map(|w| w.host_id()).unwrap_or(HostId::LOCAL)
}

/// Whether rebinding a window from `previous` to `current` moved it to another
/// machine — the moment every piece of per-*window* state that outlived the
/// swap has to be reconsidered.
pub(crate) fn crosses_machines(previous: HostId, current: HostId) -> bool {
    previous != current
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The window/host invariant, as a test.**
    ///
    /// A window is one machine. The inverse is listed under
    /// *never do this*, and the M5 data layer spends that guarantee — a
    /// workspace stores `host` once instead of per pane, and `sidebar_group`
    /// stays a bare `PathBuf` — so it has to be nailed down rather than
    /// believed.
    ///
    /// What is actually being asserted: for any view set containing local
    /// and remote entries on several machines, the host a window binds to is a
    /// *function* of the workspace it shows. Every id answers exactly one
    /// machine, and no id answers two.
    #[test]
    fn a_window_binds_to_exactly_one_machine() {
        let build = RemoteTarget::Alias {
            alias: "build-box".into(),
        };
        let gpu = RemoteTarget::direct("me", "gpu.lab", 2222);

        let local = WindowView::default();
        let build_a = WindowView::on_remote(RemoteRef::new(build.clone(), WorkspaceId::new()));
        let build_b = WindowView::on_remote(RemoteRef::new(build, WorkspaceId::new()));
        let gpu_a = WindowView::on_remote(RemoteRef::new(gpu, WorkspaceId::new()));
        let (local_id, build_a_id, build_b_id, gpu_id) =
            (local.id, build_a.id, build_b.id, gpu_a.id);

        let views = WindowViews {
            views: vec![local, build_a, build_b, gpu_a],
            ..WindowViews::default()
        };

        // Three machines are represented, and they stay apart.
        let l = host_for(&views, local_id);
        let b1 = host_for(&views, build_a_id);
        let b2 = host_for(&views, build_b_id);
        let g = host_for(&views, gpu_id);
        assert_eq!(l, HostId::LOCAL);
        assert_eq!(b1, b2, "two workspaces on one box share its connection");
        assert_ne!(b1, g);
        assert_ne!(b1, l);
        assert_ne!(g, l);

        // The answer is stable: asking twice cannot give a window a second host.
        assert_eq!(host_for(&views, build_a_id), b1);

        // And a window whose workspace was deleted underneath it falls back to
        // local rather than to some other machine's id.
        assert_eq!(host_for(&views, WorkspaceId::new()), HostId::LOCAL);

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
        let a = WindowView::on_remote(RemoteRef::new(build.clone(), WorkspaceId::new()));
        let b = WindowView::on_remote(RemoteRef::new(build, WorkspaceId::new()));
        let c = WindowView::on_remote(RemoteRef::new(other, WorkspaceId::new()));
        let local = WindowView::default();

        assert_eq!(a.host_id(), b.host_id());
        assert_ne!(a.host_id(), c.host_id());
        assert_eq!(local.host_id(), HostId::LOCAL);
        assert_ne!(a.host_id(), HostId::LOCAL);
    }
}

pub use tty7_core::core::session::{
    RemoteRef, RemoteTarget, RouteSnapshot, Session, SessionAxis, SessionPane, SessionTab,
    WindowView, WindowViews, WorkspaceId,
};
pub use tty7_core::host::HostId;

pub struct WorkspaceStore {
    views: WindowViews,
}

impl gpui::Global for WorkspaceStore {}

impl WorkspaceStore {
    pub fn init(cx: &mut gpui::App) {
        let views = WindowViews::load().unwrap_or_default();
        cx.set_global(Self { views });
    }

    #[cfg(test)]
    pub fn install_for_test(cx: &mut gpui::App, views: WindowViews) {
        cx.set_global(Self { views });
    }

    pub fn all(cx: &gpui::App) -> &WindowViews {
        static EMPTY: std::sync::OnceLock<WindowViews> = std::sync::OnceLock::new();
        match cx.try_global::<Self>() {
            Some(store) => &store.views,
            None => EMPTY.get_or_init(WindowViews::default),
        }
    }

    fn try_store(cx: &mut gpui::App) -> Option<&mut Self> {
        cx.has_global::<Self>().then(|| cx.global_mut::<Self>())
    }

    pub fn claim(cx: &mut gpui::App, id: Option<WorkspaceId>) -> WorkspaceId {
        let Some(store) = Self::try_store(cx) else {
            return WorkspaceId::new();
        };
        let view = match id {
            // A named workspace keeps its name even when this client has never
            // opened it: the CLI and other clients make workspaces too, and the
            // id in the machine's tree is the one a window has to claim. Taking
            // a fresh id here would have opened an empty stranger instead.
            Some(id) => match store.views.views.iter().position(|w| w.id == id) {
                Some(at) => &mut store.views.views[at],
                None => {
                    store.views.views.push(WindowView {
                        id,
                        ..WindowView::default()
                    });
                    store.views.views.last_mut().expect("just pushed")
                }
            },
            None => {
                store.views.views.push(WindowView::default());
                store.views.views.last_mut().expect("just pushed")
            }
        };
        view.open = true;
        view.synced = false;
        view.touch();
        let claimed = view.id;
        store.views.active = Some(claimed);
        store.views.save();
        claimed
    }

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
            return;
        };
        view.window = Some(window);
        if let Some((label, subject)) = hint {
            view.label = Some(label);
            view.subject = subject;
        }
        store.views.save();
    }

    pub fn focus(cx: &mut gpui::App, id: WorkspaceId) {
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        if let Some(view) = store.views.get_mut(id) {
            view.touch();
        }
        store.views.active = Some(id);
        store.views.save();
        crate::ui::tree_sync::fire_workspace_op(cx, id, |ws| {
            tty7_core::daemon::control::ControlRequest::WorkspaceTouch { workspace: ws }
        });
    }

    /// Restore the one window a launch reopens, detaching every other that
    /// was open at quit. The count comes back with the id: those workspaces
    /// are still running, and silence about them is how they get forgotten
    /// (#597) — the caller is expected to say something.
    pub fn restore_one(cx: &mut gpui::App) -> Option<(WorkspaceId, usize)> {
        let store = Self::try_store(cx)?;
        let keep = store.views.workspace_to_restore()?;
        let reattaching = store.views.get(keep).is_some_and(|view| !view.open);
        let mut detached = 0usize;
        for view in &mut store.views.views {
            if view.open && view.id != keep {
                view.open = false;
                detached += 1;
            }
        }
        store.views.active = Some(keep);
        store.views.save();
        if reattaching {
            log::info!("launch: no window was open at quit; reattaching the last one closed");
        } else if detached > 0 {
            log::info!("launch: restoring 1 workspace, left {detached} detached");
        }
        Some((keep, detached))
    }

    pub fn close_window(cx: &mut gpui::App, id: WorkspaceId) {
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

    pub fn host_of(cx: &gpui::App, id: WorkspaceId) -> HostId {
        host_for(Self::all(cx), id)
    }

    pub fn remote_ref(cx: &gpui::App, id: WorkspaceId) -> Option<RemoteRef> {
        Self::all(cx).get(id).and_then(|w| w.host.clone())
    }

    pub fn machine_is_connected(cx: &mut gpui::App, id: WorkspaceId) -> bool {
        let Some(host) = Self::remote_ref(cx, id) else {
            return true;
        };
        crate::ui::remote_connect::HostLinks::get(cx, host.host_id()).is_some()
    }

    pub fn claim_remote(cx: &mut gpui::App, mut host: RemoteRef) -> WorkspaceId {
        // Remember the route while it still resolves: after the profile is
        // deleted, this snapshot is the only name the entry has left (#485).
        // Every (re-)open comes through here, so a rename or a repoint is
        // picked up in time.
        if let Some(cfg) = cx.try_global::<crate::core::config::Config>() {
            host.refresh_via(&cfg.ssh_profiles);
        }
        let Some(store) = Self::try_store(cx) else {
            return WorkspaceId::new();
        };
        let existing = store
            .views
            .views
            .iter_mut()
            .find(|w| w.host.as_ref() == Some(&host));
        let id = match existing {
            Some(view) => {
                // A fresh snapshot refreshes the stored one; a `None` here
                // means the profile is already gone and the stored snapshot
                // is the last name the entry has — keep it.
                if let (Some(h), Some(via)) = (view.host.as_mut(), host.via.clone()) {
                    h.via = Some(via);
                }
                // Claimed is opened: the reference stops being a mirror of
                // someone else's listing and becomes this client's own.
                view.synced = false;
                view.id
            }
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

    /// Mirrors one machine's own workspace listing into the store at connect
    /// time, so the switcher still knows that machine's workspaces after a
    /// restart, link or no link. Listed workspaces the store has never seen
    /// get a reference marked `synced` (launch restore skips those); ones it
    /// has get their label refreshed; unopened references whose workspace has
    /// left the listing — deleted by another client — are dropped.
    pub fn sync_remote(
        cx: &mut gpui::App,
        target: &RemoteTarget,
        listing: &[(WorkspaceId, String, u64)],
    ) {
        let profiles = cx
            .try_global::<crate::core::config::Config>()
            .map(|cfg| cfg.ssh_profiles.clone())
            .unwrap_or_default();
        let Some(store) = Self::try_store(cx) else {
            return;
        };
        for (ws, name, last_active) in listing {
            let label = Some(name.trim().to_string()).filter(|n| !n.is_empty());
            match store.views.views.iter_mut().find(|w| {
                w.host
                    .as_ref()
                    .is_some_and(|h| &h.target == target && h.workspace == *ws)
            }) {
                Some(view) => {
                    if !view.open {
                        if label.is_some() {
                            view.label = label;
                        }
                        // The machine's clock only drives entries this client
                        // has never used; a used one keeps meaning "when *I*
                        // last had it open".
                        if view.synced {
                            view.last_active = *last_active;
                        }
                    }
                }
                None => {
                    let mut host = RemoteRef::new(target.clone(), *ws);
                    host.refresh_via(&profiles);
                    let mut view = WindowView::on_remote(host);
                    view.open = false;
                    view.synced = true;
                    view.label = label;
                    view.last_active = *last_active;
                    store.views.views.push(view);
                }
            }
        }
        let listed: std::collections::HashSet<WorkspaceId> =
            listing.iter().map(|(ws, ..)| *ws).collect();
        store.views.views.retain(|w| {
            let Some(host) = w.host.as_ref() else {
                return true;
            };
            &host.target != target || w.open || listed.contains(&host.workspace)
        });
        store.views.save();
    }
}

pub(crate) fn host_for(views: &WindowViews, id: WorkspaceId) -> HostId {
    views.get(id).map(|w| w.host_id()).unwrap_or(HostId::LOCAL)
}

pub(crate) fn crosses_machines(previous: HostId, current: HostId) -> bool {
    previous != current
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let l = host_for(&views, local_id);
        let b1 = host_for(&views, build_a_id);
        let b2 = host_for(&views, build_b_id);
        let g = host_for(&views, gpu_id);
        assert_eq!(l, HostId::LOCAL);
        assert_eq!(b1, b2, "two workspaces on one box share its connection");
        assert_ne!(b1, g);
        assert_ne!(b1, l);
        assert_ne!(g, l);

        assert_eq!(host_for(&views, build_a_id), b1);

        assert_eq!(host_for(&views, WorkspaceId::new()), HostId::LOCAL);

        assert!(!crosses_machines(b1, b2));
        assert!(crosses_machines(l, b1));
        assert!(crosses_machines(b1, g));
    }

    #[gpui::test]
    fn restore_one_keeps_one_window_and_reports_the_rest(cx: &mut gpui::TestAppContext) {
        // `restore_one` saves, and a test has no business writing the real views.
        let _ = tty7_core::core::config::set_config_dir(
            std::env::temp_dir().join(format!("tty7-session-test-{}", std::process::id())),
        );
        cx.update(|cx| {
            WorkspaceStore::install_for_test(cx, WindowViews::default());
            let first = WorkspaceStore::claim(cx, None);
            let _second = WorkspaceStore::claim(cx, None);
            let third = WorkspaceStore::claim(cx, None);

            let (kept, detached) =
                WorkspaceStore::restore_one(cx).expect("three open windows restore one");
            assert_eq!(kept, third, "the most recently active window wins");
            assert_eq!(
                detached, 2,
                "the other two are still running — the launch has to say so (#597)"
            );
            let views = WorkspaceStore::all(cx);
            assert!(!views.get(first).expect("first survives").open);
            assert!(views.get(third).expect("third survives").open);

            // Restoring again with the other two already detached reports
            // nothing: the notification is for the launch that did the
            // detaching, not every launch after it.
            let (_, detached) =
                WorkspaceStore::restore_one(cx).expect("the open window restores again");
            assert_eq!(detached, 0);
        });
    }

    #[gpui::test]
    fn claiming_a_workspace_the_store_never_saw_keeps_the_id_it_was_given(
        cx: &mut gpui::TestAppContext,
    ) {
        // `claim` saves, and a test has no business writing the real views.
        let _ = tty7_core::core::config::set_config_dir(
            std::env::temp_dir().join(format!("tty7-session-test-{}", std::process::id())),
        );
        cx.update(|cx| {
            WorkspaceStore::install_for_test(cx, WindowViews::default());

            // The id came off the machine tree — the CLI made this one.
            let on_the_machine = WorkspaceId::new();
            assert_eq!(
                WorkspaceStore::claim(cx, Some(on_the_machine)),
                on_the_machine,
                "a fresh id here would have opened an empty stranger instead"
            );
            assert_eq!(
                WorkspaceStore::claim(cx, Some(on_the_machine)),
                on_the_machine,
                "claiming it twice finds the entry rather than piling up"
            );
            assert_eq!(WorkspaceStore::all(cx).views.len(), 1);

            let fresh = WorkspaceStore::claim(cx, None);
            assert_ne!(fresh, on_the_machine);
            assert_eq!(WorkspaceStore::all(cx).views.len(), 2);
        });
    }

    #[gpui::test]
    fn claim_remote_remembers_the_route_and_refreshes_it_while_the_profile_lives(
        cx: &mut gpui::TestAppContext,
    ) {
        // #485: the snapshot is written at creation, refreshed on every
        // re-open while the profile still exists, and — once the profile is
        // gone — kept as the last name the entry has.
        let _ = tty7_core::core::config::set_config_dir(
            std::env::temp_dir().join(format!("tty7-session-test-{}", std::process::id())),
        );
        cx.update(|cx| {
            let profile_id = uuid::Uuid::new_v4();
            let mut profile = tty7_core::core::ssh_profile::SshProfile::new("lager");
            profile.id = profile_id;
            profile.user = "qhw".into();
            profile.host = "222.29.101.16".into();
            let mut cfg = crate::core::config::Config::default();
            cfg.ssh_profiles.push(profile);
            cx.set_global(cfg);
            WorkspaceStore::install_for_test(cx, WindowViews::default());

            let target = RemoteTarget::Profile { id: profile_id };
            let machine_ws = WorkspaceId::new();
            let entry =
                WorkspaceStore::claim_remote(cx, RemoteRef::new(target.clone(), machine_ws));
            let via = WorkspaceStore::all(cx)
                .get(entry)
                .and_then(|w| w.host.as_ref())
                .and_then(|h| h.via.clone())
                .expect("creation writes the snapshot");
            assert_eq!(via.label(), "lager");
            assert_eq!(via.endpoint(), "qhw@222.29.101.16");

            // A rename lands on the next open, without piling up entries.
            cx.global_mut::<crate::core::config::Config>().ssh_profiles[0].name =
                "lager-renamed".into();
            let again =
                WorkspaceStore::claim_remote(cx, RemoteRef::new(target.clone(), machine_ws));
            assert_eq!(again, entry);
            assert_eq!(WorkspaceStore::all(cx).views.len(), 1);
            let via = WorkspaceStore::all(cx)
                .get(entry)
                .and_then(|w| w.host.as_ref())
                .and_then(|h| h.via.clone())
                .unwrap();
            assert_eq!(via.label(), "lager-renamed");

            // Once the profile is gone, the stored snapshot survives a
            // re-open with a via-less ref.
            cx.global_mut::<crate::core::config::Config>()
                .ssh_profiles
                .clear();
            let orphaned = WorkspaceStore::claim_remote(cx, RemoteRef::new(target, machine_ws));
            assert_eq!(orphaned, entry);
            let via = WorkspaceStore::all(cx)
                .get(entry)
                .and_then(|w| w.host.as_ref())
                .and_then(|h| h.via.clone())
                .unwrap();
            assert_eq!(via.label(), "lager-renamed", "the last name is kept");
        });
    }

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

    #[gpui::test]
    fn connect_time_sync_mirrors_the_machines_listing(cx: &mut gpui::TestAppContext) {
        // `sync_remote` saves; a test has no business writing the real views.
        let _ = tty7_core::core::config::set_config_dir(
            std::env::temp_dir().join(format!("tty7-session-test-{}", std::process::id())),
        );
        cx.update(|cx| {
            WorkspaceStore::install_for_test(cx, WindowViews::default());
            let target = RemoteTarget::direct("me", "devbox", 22);
            let elsewhere = RemoteTarget::direct("me", "gpu-lab", 22);
            let (a, b, c) = (WorkspaceId::new(), WorkspaceId::new(), WorkspaceId::new());

            // A workspace on another machine must never be touched by this
            // machine's sync.
            let kept = WorkspaceStore::claim_remote(cx, RemoteRef::new(elsewhere, c));

            WorkspaceStore::sync_remote(
                cx,
                &target,
                &[(a, "api".into(), 30), (b, "web".into(), 20)],
            );
            let synced: Vec<_> = WorkspaceStore::all(cx)
                .views
                .iter()
                .filter(|w| w.synced)
                .collect();
            assert_eq!(synced.len(), 2, "both listed workspaces gain a reference");
            assert!(
                synced.iter().all(|w| !w.open),
                "a mirrored reference is not an open window"
            );

            // The next listing dropped `b` (deleted by another client) and
            // renamed `a`: the reference set follows the machine.
            WorkspaceStore::sync_remote(cx, &target, &[(a, "api-v2".into(), 50)]);
            let store = WorkspaceStore::all(cx);
            let of_target: Vec<_> = store
                .views
                .iter()
                .filter(|w| w.host.as_ref().is_some_and(|h| h.workspace == a))
                .collect();
            assert_eq!(of_target.len(), 1);
            assert_eq!(of_target[0].label.as_deref(), Some("api-v2"));
            assert_eq!(of_target[0].last_active, 50, "a synced clock follows");
            assert!(
                !store
                    .views
                    .iter()
                    .any(|w| w.host.as_ref().is_some_and(|h| h.workspace == b)),
                "a workspace the machine no longer lists is dropped"
            );
            assert!(
                store.get(kept).is_some(),
                "another machine's entries are not this sync's to prune"
            );

            // Claiming the reference makes it this client's own: the mark
            // clears, and later syncs stop driving its clock.
            let local = WorkspaceStore::claim_remote(cx, RemoteRef::new(target.clone(), a));
            assert!(!WorkspaceStore::all(cx).get(local).expect("claimed").synced);
            WorkspaceStore::sync_remote(cx, &target, &[(a, "api-v3".into(), 99)]);
            let view = WorkspaceStore::all(cx).get(local).expect("still there");
            assert_eq!(
                view.label.as_deref(),
                Some("api-v3"),
                "the name still follows the machine"
            );
            assert_eq!(view.last_active, 50, "the clock is now this client's own");
        });
    }
}

pub use tty7_core::core::session::{
    RemoteRef, RemoteTarget, Session, SessionAxis, SessionPane, SessionTab, WindowView,
    WindowViews, WorkspaceId,
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

    pub fn restore_one(cx: &mut gpui::App) -> Option<WorkspaceId> {
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
        Some(keep)
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

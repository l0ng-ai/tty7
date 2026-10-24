use super::*;
use crate::core::config::Config;
use crate::core::session::{RemoteRef, WindowView, WindowViews};
use crate::core::ssh_profile::SshProfile;
use crate::terminal::{PaneRoute, RemoteTerminal, TermSize};
use tty7_core::daemon::control::{
    CONTROL_VERSION, ControlClientMsg, ControlHello, ControlHelloOk, ControlServerMsg,
};

fn install_view(cx: &mut gpui::App, target: RemoteTarget, remote: WorkspaceId) -> WorkspaceId {
    let view = WindowView::on_remote(RemoteRef::new(target, remote));
    let local = view.id;
    assert_ne!(
        local, remote,
        "window and server workspace are distinct identities"
    );
    WorkspaceStore::install_for_test(
        cx,
        WindowViews {
            views: vec![view],
            active: Some(local),
        },
    );
    local
}

fn peer(target: &RemoteTarget, forwarding: bool) -> Arc<RemoteHost> {
    let (client, mut server) = std::os::unix::net::UnixStream::pair().unwrap();
    std::thread::spawn(move || {
        assert!(matches!(
            ControlClientMsg::read(&mut server).unwrap(),
            ControlClientMsg::Hello(_)
        ));
        ControlServerMsg::HelloOk(ControlHelloOk {
            control_version: CONTROL_VERSION,
            protocol_version: crate::daemon::protocol::PROTOCOL_VERSION,
            build: env!("CARGO_PKG_VERSION").into(),
            separator: '/',
            home: "/tmp".into(),
            features: if forwarding {
                vec![crate::daemon::control::feature::WORKSPACE_AGENT.into()]
            } else {
                Vec::new()
            },
            instance: "agent-routing-test".into(),
        })
        .encode(&mut server)
        .unwrap();
        // Keep the peer alive until the test closes its client.
        while ControlClientMsg::read(&mut server).is_ok() {}
    });
    RemoteHost::over_unix(
        client,
        &target.connection_key(),
        &ControlHello::host_rpc("test", "test"),
    )
    .unwrap()
}

#[gpui::test]
fn gui_agent_route_uses_the_remote_workspace_identity(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        let mut profile = SshProfile::new("test-host");
        profile.host = "test.invalid".into();
        profile.agent_forward = true;
        let target = RemoteTarget::Profile { id: profile.id };
        let mut config = Config::default();
        config.ssh_profiles.push(profile);
        cx.set_global(config);
        let remote = WorkspaceId::new();
        let local = install_view(cx, target.clone(), remote);
        let host = peer(&target, true);
        remote_connect::HostLinks::insert(cx, host.clone(), "/tmp".into());

        let pane = pane_workspace_for(cx, local).unwrap();
        assert_eq!(pane.workspace, local, "UI keeps the local window identity");
        assert_eq!(pane.agent_workspace, Some(remote));
        assert!(matches!(
            pane_route_for(cx, local),
            PaneRoute::Remote { agent_workspace: Some(id), .. } if id == remote
        ));

        cx.global_mut::<Config>().ssh_profiles[0].agent_forward = false;
        assert_eq!(pane_workspace_for(cx, local).unwrap().agent_workspace, None);
        cx.global_mut::<Config>().ssh_profiles[0].agent_forward = true;
        host.client().close();
        let old = peer(&target, false);
        remote_connect::HostLinks::insert(cx, old.clone(), "/tmp".into());
        assert_eq!(pane_workspace_for(cx, local).unwrap().agent_workspace, None);
        old.client().close();
    });
}

/// Exercises the production GUI mapping and pane spawn against a real host,
/// using the running local daemon. Creates only its own remote workspace/pane.
/// TTY7_LIVE_SSH_HOST/PORT/USER select an already trusted, reachable host.
#[gpui::test]
#[ignore = "requires an explicitly selected SSH host, local daemon and loaded SSH agent"]
fn live_gui_workspace_agent_lists_identities(cx: &mut gpui::TestAppContext) {
    // Use the real daemon's transport while keeping every test config read or
    // write in scratch. Never launch a daemon against the user's config here.
    let live = crate::core::config::config_dir_path().expect("local daemon config directory");
    std::os::unix::net::UnixStream::connect(live.join("daemon.sock"))
        .expect("start the local tty7 daemon before this opt-in test");
    let scratch = tempfile::tempdir_in("/tmp").unwrap();
    for name in ["daemon.sock", "control.sock"] {
        std::os::unix::fs::symlink(live.join(name), scratch.path().join(name)).unwrap();
    }
    crate::core::config::set_config_dir(scratch.path().to_path_buf());
    let (target, header) = cx.update(|cx| {
        let mut profile = SshProfile::new("gui-agent-live-test");
        profile.host = std::env::var("TTY7_LIVE_SSH_HOST").expect("set TTY7_LIVE_SSH_HOST");
        profile.port = std::env::var("TTY7_LIVE_SSH_PORT")
            .expect("set TTY7_LIVE_SSH_PORT")
            .parse()
            .unwrap();
        profile.user = std::env::var("TTY7_LIVE_SSH_USER").expect("set TTY7_LIVE_SSH_USER");
        profile.agent_forward = true;
        let target = RemoteTarget::Profile { id: profile.id };
        let mut config = Config::default();
        config.ssh_profiles.push(profile);
        cx.set_global(config);
        let header = remote_connect::control_route(&target, cx).unwrap();
        (target, header)
    });
    let connecting = target.clone();
    let connected = std::thread::spawn(move || {
        remote_connect::connect_blocking(&connecting, header, "gui-agent-live-test")
    })
    .join()
    .unwrap()
    .unwrap();
    assert!(
        connected.agent_warning.is_none(),
        "{:?}",
        connected.agent_warning
    );
    let host = connected.host.clone();
    let remote = match host
        .client()
        .call(ControlRequest::WorkspaceCreate {
            name: Some("gui-agent-live-test".into()),
            workspace: None,
        })
        .unwrap()
    {
        ReplyOk::WorkspaceTree(ws) => ws.id,
        other => panic!("unexpected create reply: {other:?}"),
    };
    let (local, route) = cx.update(|cx| {
        let local = install_view(cx, target, remote);
        remote_connect::HostLinks::insert(cx, connected.host, connected.home);
        (local, pane_route_for(cx, local))
    });
    // Cleanup runs on failures as well, and never touches another workspace.
    struct Cleanup {
        host: Arc<RemoteHost>,
        workspace: WorkspaceId,
        pane: Option<(PaneRoute, u64)>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some((route, pane)) = &self.pane {
                RemoteTerminal::kill_pane_on(route, *pane);
            }
            let _ = self.host.client().call(ControlRequest::WorkspaceRemove {
                workspace: self.workspace,
            });
            self.host.client().close();
        }
    }
    let mut cleanup = Cleanup {
        host: host.clone(),
        workspace: remote,
        pane: None,
    };
    host.client()
        .call(ControlRequest::WorkspaceAttach {
            id: remote.to_string(),
        })
        .unwrap();
    let spawn_route = route.clone();
    let (terminal, id) = std::thread::spawn(move || {
        RemoteTerminal::spawn_on(
            &spawn_route,
            TermSize::new(120, 30),
            8,
            17,
            None,
            Some(crate::daemon::protocol::ShellSpec {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "ssh-add -L >/dev/null; printf '\\nTTY7_AGENT_EXIT:%s\\n' \"$?\"; sleep 30"
                        .into(),
                ],
                args_are_tty7_defaults: false,
            }),
            Some(local.to_string()),
            None,
        )
    })
    .join()
    .unwrap()
    .unwrap();
    cleanup.pane = Some((route, id));
    let deadline = Instant::now() + Duration::from_secs(15);
    let seen = loop {
        let seen: String = terminal
            .term
            .lock()
            .grid()
            .display_iter()
            .map(|cell| cell.c)
            .collect();
        if seen.contains("TTY7_AGENT_EXIT:") {
            break seen;
        }
        assert!(Instant::now() < deadline, "ssh-add did not finish: {seen}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        seen.contains("TTY7_AGENT_EXIT:0"),
        "ssh-add failed in the production GUI spawn path: {seen}"
    );
    println!("PASS GUI mapping -> local daemon -> real SSH -> remote workspace pane -> ssh-add -L");
    drop(terminal);
    drop(cleanup);
}

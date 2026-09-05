use std::io;
use std::sync::{Arc, RwLockReadGuard};

use crate::core::session::WorkspaceId;
use crate::daemon::control::LinkShutdown;
use crate::daemon::protocol::PaneAccess;
use crate::daemon::transport::Stream;
use crate::host::ownership::PaneLease;

pub(super) enum Authority {
    ReadOnly,
    Manage,
    Workspace {
        workspace: WorkspaceId,
        lease: Arc<PaneLease>,
        _closer: Arc<dyn LinkShutdown>,
    },
}

impl Authority {
    pub(super) fn open(
        access: Option<PaneAccess>,
        registry: &super::Registry,
        stream: &Stream,
    ) -> io::Result<Self> {
        match access {
            None => Ok(Self::ReadOnly),
            Some(PaneAccess::Manage) => Ok(Self::Manage),
            Some(PaneAccess::Workspace(auth)) => {
                let lease = registry.attachments.pane_lease(&auth)?;
                let closer: Arc<dyn LinkShutdown> = Arc::new(stream.try_clone()?);
                lease.register(&closer)?;
                Ok(Self::Workspace {
                    workspace: auth.workspace,
                    lease,
                    _closer: closer,
                })
            }
        }
    }

    pub(super) fn enter(&self) -> io::Result<Option<RwLockReadGuard<'_, bool>>> {
        match self {
            Self::ReadOnly => Err(denied(
                "pane writes require workspace access or explicit same-user management",
            )),
            Self::Manage => Ok(None),
            Self::Workspace { lease, .. } => lease.enter().map(Some),
        }
    }

    pub(super) fn workspace(&self) -> Option<WorkspaceId> {
        match self {
            Self::Workspace { workspace, .. } => Some(*workspace),
            _ => None,
        }
    }

    pub(super) fn require_management(&self) -> io::Result<()> {
        match self {
            Self::Manage => Ok(()),
            _ => Err(denied(
                "this operation requires explicit same-user management",
            )),
        }
    }

    /// A live pane is bound to one workspace for its lifetime. Newly spawned
    /// panes get their binding before publication; pre-existing panes can be
    /// adopted only from an unambiguous authoritative tree reference. A tree
    /// copy/reference is not permission to seize another workspace's pane.
    pub(super) fn check_pane(&self, registry: &super::Registry, pane: u64) -> io::Result<()> {
        let Some(workspace) = self.workspace() else {
            return Ok(());
        };
        let mut bindings = registry.bindings.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(owner) = bindings.get(&pane) {
            return if *owner == workspace {
                Ok(())
            } else {
                Err(denied("pane belongs to another workspace"))
            };
        }
        let machine = registry
            .machine
            .get()
            .ok_or_else(|| denied("workspace pane binding is unavailable"))?;
        if machine.unique_workspace_for_pane(pane) != Some(workspace) {
            return Err(denied("pane is not uniquely owned by this workspace"));
        }
        bindings.insert(pane, workspace);
        Ok(())
    }

    pub(super) fn bind_spawn(&self, registry: &super::Registry, pane: u64) {
        if let Some(workspace) = self.workspace() {
            registry
                .bindings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pane, workspace);
        }
    }
}

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::core::machine::{MachineStore, PaneSeed};
    use crate::daemon::control::{
        ControlClient, ControlHello, ControlRequest, ReplyOk, WorkspaceProof,
    };
    use crate::daemon::protocol::{ClientMsg, DaemonMsg, PaneAuthorization, ShellSpec, WinSize};
    use crate::host::server::{Services, serve_with};
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    const SIZE: WinSize = WinSize {
        cols: 80,
        rows: 24,
        cell_w: 8,
        cell_h: 16,
    };

    struct Fixture {
        registry: Arc<super::super::Registry>,
        services: Services,
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let machine = MachineStore::open(dir.path().join("machine.json"));
            let registry = Arc::new(super::super::Registry::new());
            registry.machine.set(machine.clone()).ok().unwrap();
            let services = Services {
                machine: Some(machine),
                attachments: registry.attachments.clone(),
                panes: Some(registry.clone()),
            };
            Self {
                registry,
                services,
                _dir: dir,
            }
        }

        fn workspace(&self) -> WorkspaceId {
            self.services
                .machine
                .as_ref()
                .unwrap()
                .workspace_create(None, None, None)
                .unwrap()
                .id
        }

        fn control(&self) -> ControlClient {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let services = self.services.clone();
            std::thread::spawn(move || {
                let (server, _) = listener.accept().unwrap();
                let _ = serve_with(server, crate::host::local::LocalHost::new(), services);
            });
            ControlClient::over_tcp(
                TcpStream::connect(address).unwrap(),
                &ControlHello::host_rpc("same-client", "test"),
                Box::new(|_| {}),
            )
            .unwrap()
        }

        fn open(&self, access: Option<PaneAccess>, request: ClientMsg) -> UnixStream {
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let registry = self.registry.clone();
            std::thread::spawn(move || {
                let _ = super::super::handle_conn(server, registry);
            });
            let mut wire = Vec::new();
            if let Some(access) = access {
                ClientMsg::Access(access).encode(&mut wire).unwrap();
            }
            request.encode(&mut wire).unwrap();
            client.write_all(&wire).unwrap();
            client
        }

        fn spawn(&self, auth: &PaneAuthorization) -> (UnixStream, u64) {
            let mut stream = self.open(Some(PaneAccess::Workspace(auth.clone())), spawn_request());
            let DaemonMsg::Spawned { pane_id } = DaemonMsg::read(&mut stream).unwrap() else {
                panic!("spawn rejected");
            };
            assert_eq!(
                self.registry.get(pane_id).unwrap().info().owner.as_deref(),
                Some(auth.workspace.to_string().as_str())
            );
            (stream, pane_id)
        }

        fn refused(&self, access: Option<PaneAccess>, request: ClientMsg) {
            let mut stream = self.open(access, request);
            assert!(matches!(
                DaemonMsg::read(&mut stream).unwrap(),
                DaemonMsg::Error(_)
            ));
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.registry.drain_and_kill();
        }
    }

    fn spawn_request() -> ClientMsg {
        ClientMsg::Spawn {
            cwd: Some(std::env::temp_dir()),
            size: SIZE,
            shell: Some(ShellSpec {
                program: "/bin/cat".into(),
                args: Vec::new(),
                args_are_tty7_defaults: false,
            }),
            owner: Some("untrusted-owner-label".into()),
            workspace: None,
            restore: None,
            allow_remote_clipboard_write: false,
        }
    }

    fn acquire(
        client: &ControlClient,
        workspace: WorkspaceId,
        proof: Option<WorkspaceProof>,
        takeover: bool,
    ) -> (WorkspaceProof, PaneAuthorization) {
        let request = if takeover {
            ControlRequest::WorkspaceTakeOver {
                id: workspace.to_string(),
            }
        } else {
            ControlRequest::WorkspaceResume {
                id: workspace.to_string(),
                proof,
            }
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let reply = loop {
            match client.call(request.clone()) {
                Err(error)
                    if error
                        .to_string()
                        .contains("pane command is still in flight")
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                result => break result.unwrap(),
            }
        };
        let ReplyOk::WorkspaceLease {
            proof, pane_token, ..
        } = reply
        else {
            panic!("lease refused");
        };
        (
            proof,
            PaneAuthorization {
                workspace,
                token: pane_token,
            },
        )
    }

    fn attach(pane_id: u64) -> ClientMsg {
        ClientMsg::Attach {
            pane_id,
            size: SIZE,
            allow_remote_clipboard_write: false,
        }
    }

    #[test]
    fn takeover_revokes_live_streams_and_all_old_capability_openings_without_killing_the_pty() {
        let fixture = Fixture::new();
        let workspace = fixture.workspace();
        let a = fixture.control();
        let b = fixture.control();
        let (_, old) = acquire(&a, workspace, None, false);
        let (mut stream, pane) = fixture.spawn(&old);
        let process = fixture.registry.get(pane).unwrap();
        let (proof, current) = acquire(&b, workspace, None, true);
        // Buffered output may precede EOF, but a successful takeover closes
        // the old stream even if that client never reads the control notice.
        loop {
            match DaemonMsg::read(&mut stream) {
                Ok(_) => {}
                Err(error) => {
                    assert!(!matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ));
                    break;
                }
            }
        }
        assert!(process.alive());
        assert!(Arc::ptr_eq(&process, &fixture.registry.get(pane).unwrap()));
        for request in [
            attach(pane),
            spawn_request(),
            ClientMsg::SendInput {
                pane_id: pane,
                bytes: b"stale\n".to_vec(),
            },
            ClientMsg::Kill { pane_id: pane },
        ] {
            fixture.refused(Some(PaneAccess::Workspace(old.clone())), request);
        }
        // The long-lived resume proof is deliberately NOT a pane credential.
        fixture.refused(
            Some(PaneAccess::Workspace(PaneAuthorization {
                workspace,
                token: proof.clone(),
            })),
            attach(pane),
        );
        let mut current_stream =
            fixture.open(Some(PaneAccess::Workspace(current.clone())), attach(pane));
        assert!(matches!(
            DaemonMsg::read(&mut current_stream).unwrap(),
            DaemonMsg::Size(_)
        ));
        ClientMsg::Input(b"current owner\n".to_vec())
            .encode(&mut current_stream)
            .unwrap();
        let mut output = Vec::new();
        while !output
            .windows(b"current owner".len())
            .any(|bytes| bytes == b"current owner")
        {
            match DaemonMsg::read(&mut current_stream).unwrap() {
                DaemonMsg::Output(bytes) => output.extend(bytes),
                DaemonMsg::Exited { .. } => panic!("the original process died"),
                _ => {}
            }
        }
        // Resuming with the same durable proof rotates the pane capability.
        let (_, renewed) = acquire(&b, workspace, Some(proof), false);
        assert_ne!(renewed.token, current.token);
        fixture.refused(Some(PaneAccess::Workspace(current)), attach(pane));
        let mut resumed = fixture.open(Some(PaneAccess::Workspace(renewed)), attach(pane));
        assert!(matches!(
            DaemonMsg::read(&mut resumed).unwrap(),
            DaemonMsg::Size(_)
        ));
        assert!(process.alive());
    }

    #[test]
    fn workspace_capabilities_cannot_cross_pane_boundaries_or_downgrade_to_unscoped_writes() {
        let fixture = Fixture::new();
        let a = fixture.control();
        let b = fixture.control();
        let (_, own) = acquire(&a, fixture.workspace(), None, false);
        let (_, other) = acquire(&b, fixture.workspace(), None, false);
        let (_stream, pane) = fixture.spawn(&own);
        for request in [
            attach(pane),
            ClientMsg::SendInput {
                pane_id: pane,
                bytes: b"wrong workspace\n".to_vec(),
            },
            ClientMsg::Kill { pane_id: pane },
        ] {
            fixture.refused(Some(PaneAccess::Workspace(other.clone())), request);
        }
        for request in [
            spawn_request(),
            attach(pane),
            ClientMsg::SendInput {
                pane_id: pane,
                bytes: b"legacy\n".to_vec(),
            },
            ClientMsg::Kill { pane_id: pane },
            ClientMsg::Shutdown,
        ] {
            fixture.refused(None, request);
        }
        fixture.refused(
            Some(PaneAccess::Workspace(own.clone())),
            ClientMsg::Shutdown,
        );
        assert!(fixture.registry.get(pane).unwrap().alive());
        // Explicit administration by this same OS account is a separate API.
        let mut manage = fixture.open(
            Some(PaneAccess::Manage),
            ClientMsg::SendInput {
                pane_id: pane,
                bytes: b"administrator\n".to_vec(),
            },
        );
        assert_eq!(
            DaemonMsg::read(&mut manage).unwrap(),
            DaemonMsg::InputAck { pane_id: pane }
        );
    }

    #[test]
    fn removing_a_workspace_waits_for_inflight_commands_and_revokes_without_implicitly_killing() {
        let fixture = Fixture::new();
        let client = fixture.control();
        let workspace = fixture.workspace();
        let (_, access) = acquire(&client, workspace, None, false);
        let (_stream, pane) = fixture.spawn(&access);
        let lease = fixture.registry.attachments.pane_lease(&access).unwrap();
        let admitted = lease.enter().unwrap();
        assert!(
            client
                .call(ControlRequest::WorkspaceRemove { workspace })
                .is_err()
        );
        assert!(
            fixture
                .services
                .machine
                .as_ref()
                .unwrap()
                .workspace(workspace)
                .is_ok()
        );
        drop(admitted);
        assert!(
            lease.enter().is_ok(),
            "a refused removal does not revoke the owner"
        );
        client
            .call(ControlRequest::WorkspaceRemove { workspace })
            .unwrap();
        fixture.refused(
            Some(PaneAccess::Workspace(access)),
            ClientMsg::Kill { pane_id: pane },
        );
        assert!(
            fixture.registry.get(pane).unwrap().alive(),
            "background tree sync must not become an implicit process kill"
        );
    }

    #[test]
    fn acknowledged_kills_finish_before_workspace_removal_invalidates_their_capability() {
        let fixture = Fixture::new();
        let client = fixture.control();
        let workspace = fixture.workspace();
        let (_, access) = acquire(&client, workspace, None, false);
        let (_stream, pane) = fixture.spawn(&access);
        for _ in 0..2 {
            let mut kill = fixture.open(
                Some(PaneAccess::Workspace(access.clone())),
                ClientMsg::Kill { pane_id: pane },
            );
            assert_eq!(
                DaemonMsg::read(&mut kill).unwrap(),
                DaemonMsg::KillAck { pane_id: pane }
            );
            assert!(fixture.registry.get(pane).is_none());
        }
        client
            .call(ControlRequest::WorkspaceRemove { workspace })
            .unwrap();
        fixture.refused(Some(PaneAccess::Workspace(access)), spawn_request());
    }

    #[test]
    fn an_unscoped_preexisting_pane_can_be_adopted_only_from_its_unique_tree_owner() {
        let fixture = Fixture::new();
        let mut managed = fixture.open(Some(PaneAccess::Manage), spawn_request());
        let DaemonMsg::Spawned { pane_id } = DaemonMsg::read(&mut managed).unwrap() else {
            panic!("spawn");
        };
        let a = fixture.control();
        let b = fixture.control();
        let (_, own) = acquire(&a, fixture.workspace(), None, false);
        let (_, other) = acquire(&b, fixture.workspace(), None, false);
        fixture.refused(Some(PaneAccess::Workspace(own.clone())), attach(pane_id));
        let machine = fixture.services.machine.as_ref().unwrap();
        machine
            .tab_create(own.workspace, None, PaneSeed::bare(pane_id), None, None)
            .unwrap();
        fixture.refused(Some(PaneAccess::Workspace(other.clone())), attach(pane_id));
        let mut stream = fixture.open(Some(PaneAccess::Workspace(own.clone())), attach(pane_id));
        assert!(matches!(
            DaemonMsg::read(&mut stream).unwrap(),
            DaemonMsg::Size(_)
        ));
        assert!(
            machine
                .tab_create(other.workspace, None, PaneSeed::bare(pane_id), None, None)
                .is_err()
        );
        fixture.refused(Some(PaneAccess::Workspace(other)), attach(pane_id));
    }
}

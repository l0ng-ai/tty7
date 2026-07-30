//! The machine-owned workspace tree, end to end against a real `tty7-server`
//! child process.
//!
//! The client is the shipped `ControlClient`, the wire is the control dialect
//! over real pipes, and the server is the shipped binary owning its tree in a
//! file. What the process boundary buys here specifically:
//!
//! | | Why an in-process store would not do |
//! |---|---|
//! | The tree is on **the server's** disk | The whole design is "the daemon owns the structure"; a store in the test's address space proves the data type, not the ownership |
//! | `machine-tree` is advertised only when served | The capability bit is built from what the *binary* wires up |
//! | A delta reaches the **other** connection, never the writer | Origin exclusion is the contract that lets a client apply its own edit from the reply and everyone else's from the push |
//!
//! Every case gets its own `$TTY7_DATA_DIR`, so no case can be explained by
//! another's leftovers and nothing here can touch a developer's real tree.

// Unix-only: the server under test is a `--stdio` child, and the two-client
// case stands up a control socket.
#![cfg(unix)]

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tty7_core::core::machine::{Axis, LayoutDelta, MACHINE_FILE, PaneNode, PaneSeed};
use tty7_core::daemon::control::{
    ControlClient, ControlEvent, ControlHello, ControlRequest, LinkShutdown, ReplyOk, WorkspaceId,
    feature,
};

/// The child, and the only way to end it — a process-backed link is reaped by
/// its `LinkShutdown`, exactly as in `stdio_conformance.rs`.
struct ServerProcess {
    child: Mutex<Option<Child>>,
}

impl LinkShutdown for ServerProcess {
    fn shutdown_link(&self) -> io::Result<()> {
        let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return Ok(());
        };
        let _ = child.kill();
        let _ = child.wait();
        Ok(())
    }
}

/// One connected client: the RPC channel, plus everything the server pushed.
struct Client {
    control: ControlClient,
    events: Arc<Mutex<Vec<ControlEvent>>>,
    peer_features: Vec<String>,
}

impl Client {
    /// Wait for a `Layout` delta about `workspace` matching `want`, or fail
    /// saying what did arrive. Polled because a push and the reply that caused
    /// it race by construction.
    fn expect_delta(&self, workspace: WorkspaceId, want: impl Fn(&LayoutDelta) -> bool) {
        let key = workspace.to_string();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let seen = self
                .events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if seen.iter().any(|e| {
                matches!(e, ControlEvent::Layout { workspace: w, delta } if *w == key && want(delta))
            }) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no matching Layout delta for {key}; saw {seen:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn delta_count(&self) -> usize {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|e| matches!(e, ControlEvent::Layout { .. }))
            .count()
    }
}

/// Start a `tty7-server --stdio --serve` whose tree lives in `data_dir`, and
/// connect a client to it. `--serve` for the same reason as everywhere else in
/// these tests: a developer's real daemon must never be bridged into.
fn connect(data_dir: &Path, token: &str) -> Client {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tty7-server"))
        .args(["--stdio", "--serve"])
        .env("TTY7_DATA_DIR", data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("could not start tty7-server --stdio");

    let stdout = child.stdout.take().expect("piped");
    let stdin = child.stdin.take().expect("piped");
    let closer: Arc<dyn LinkShutdown> = Arc::new(ServerProcess {
        child: Mutex::new(Some(child)),
    });

    let events: Arc<Mutex<Vec<ControlEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let control = ControlClient::connect_with(
        stdout,
        stdin,
        Some(closer),
        &ControlHello::host_rpc(token, "test-client"),
        Box::new(move |event| sink.lock().unwrap_or_else(|e| e.into_inner()).push(event)),
    )
    .expect("handshake with tty7-server --stdio");

    let peer_features = control.hello().features.clone();
    Client {
        control,
        events,
        peer_features,
    }
}

fn data_dir() -> tempfile::TempDir {
    tempfile::TempDir::new().unwrap()
}

fn machine_file(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join(MACHINE_FILE)
}

fn seed(pane: u64, cwd: &str) -> PaneSeed {
    PaneSeed {
        pane,
        cwd: Some(cwd.to_string()),
        ssh_spec: None,
        agent: None,
    }
}

// ---------------------------------------------------------------------------

/// The capability bit is the client's cue that the tree verbs are worth a
/// round trip, and it has to reflect what the shipped binary wired up.
#[test]
fn the_server_advertises_the_machine_tree() {
    let dir = data_dir();
    let client = connect(dir.path(), "cap");
    assert!(
        client
            .peer_features
            .iter()
            .any(|f| f == feature::MACHINE_TREE),
        "features were {:?}",
        client.peer_features
    );
}

/// The semantic operations against a real server, and the tree ends up in a
/// file that server owns. This is "the daemon owns the structure" as a
/// syscall someone else made, not as a diagram.
#[test]
fn the_tree_is_built_by_operations_and_lives_in_the_servers_file() {
    let dir = data_dir();
    let client = connect(dir.path(), "ops");

    // Build: a workspace, a tab, a split.
    let ws = match client
        .control
        .call(ControlRequest::WorkspaceCreate {
            name: Some("api".into()),
            workspace: None,
        })
        .expect("create workspace")
    {
        ReplyOk::WorkspaceTree(ws) => *ws,
        other => panic!("expected WorkspaceTree, got {other:?}"),
    };
    let tab = match client
        .control
        .call(ControlRequest::TabCreate {
            workspace: ws.id,
            at: None,
            pane: seed(1, "/home/me/proj"),
            tab: None,
        })
        .expect("create tab")
    {
        ReplyOk::TabTree(tab) => *tab,
        other => panic!("expected TabTree, got {other:?}"),
    };
    client
        .control
        .call(ControlRequest::PaneSplit {
            workspace: ws.id,
            pane: 1,
            axis: Axis::Vertical,
            ratio: 0.3,
            new: seed(2, "/home/me/proj/sub"),
            first: false,
        })
        .expect("split");

    // Read back through the wire.
    let machine = match client.control.call(ControlRequest::MachineGet).unwrap() {
        ReplyOk::MachineTree(m) => *m,
        other => panic!("expected MachineTree, got {other:?}"),
    };
    assert_eq!(machine.workspaces.len(), 1);
    assert_eq!(machine.workspaces[0].tabs[0].id, tab.id);
    assert_eq!(machine.workspaces[0].tabs[0].root.pane_ids(), vec![1, 2]);
    assert_eq!(machine.panes.len(), 2);
    assert!(
        machine.panes.iter().all(|p| p.live),
        "panes this server was told about in its own lifetime are live"
    );

    // The file is the server's: the test process never wrote it.
    let text = std::fs::read_to_string(machine_file(&dir)).expect("the server wrote its tree");
    assert!(text.contains(&ws.id.to_string()), "{text}");

    // A refusal is a client-visible error, not a dropped reply.
    let missing = client
        .control
        .call(ControlRequest::WorkspaceTree {
            workspace: WorkspaceId::new(),
        })
        .unwrap_err();
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);
}

/// **The revival contract, across a real restart.** A second server process
/// reads the first one's tree; every pane in it is dead (`live == false`), the
/// leaves still name them, and `PaneReplace` rebinds a leaf to a successor.
#[test]
fn a_new_server_process_reports_the_old_panes_dead_and_accepts_their_successors() {
    let dir = data_dir();
    let ws = {
        let first = connect(dir.path(), "first");
        let ws = match first
            .control
            .call(ControlRequest::WorkspaceCreate {
                name: None,
                workspace: None,
            })
            .unwrap()
        {
            ReplyOk::WorkspaceTree(ws) => *ws,
            other => panic!("{other:?}"),
        };
        first
            .control
            .call(ControlRequest::TabCreate {
                workspace: ws.id,
                at: None,
                pane: seed(7, "/home/me/proj"),
                tab: None,
            })
            .unwrap();
        first.control.close();
        ws
    };

    // A brand-new server process over the same file.
    let second = connect(dir.path(), "second");
    let machine = match second.control.call(ControlRequest::MachineGet).unwrap() {
        ReplyOk::MachineTree(m) => *m,
        other => panic!("{other:?}"),
    };
    let record = machine
        .panes
        .iter()
        .find(|p| p.id == 7)
        .expect("the pane record survives the restart");
    assert!(!record.live, "a restarted server has no live panes");
    assert_eq!(
        record.cwd.as_deref(),
        Some("/home/me/proj"),
        "the facts a successor spawns from survive"
    );
    assert_eq!(
        machine.workspaces[0].tabs[0].root,
        PaneNode::Leaf { pane: 7 },
        "the leaf still names the dead pane — the revival slot"
    );

    // Revive: a fresh pane takes the leaf, the spent record goes.
    second
        .control
        .call(ControlRequest::PaneReplace {
            workspace: ws.id,
            old: 7,
            new: seed(1, "/home/me/proj"),
        })
        .expect("replace");
    let machine = match second.control.call(ControlRequest::MachineGet).unwrap() {
        ReplyOk::MachineTree(m) => *m,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        machine.workspaces[0].tabs[0].root,
        PaneNode::Leaf { pane: 1 }
    );
    assert!(machine.panes.iter().all(|p| p.id != 7));
}

/// Two clients on one server. An operation by one reaches the other as a
/// `Layout` delta and never comes back to its author — the mechanism that
/// replaces whole-record last-writer-wins with edits that all land.
#[test]
fn an_operation_from_one_client_reaches_the_other_as_a_delta() {
    use tty7_core::host::local::LocalHost;
    use tty7_core::host::server;

    let dir = data_dir();
    let machine = tty7_core::core::machine::MachineStore::open(machine_file(&dir));
    let sock = dir.path().join("control.sock");
    let listener = server::bind_control_socket(&sock).unwrap();
    {
        let machine = Arc::clone(&machine);
        std::thread::spawn(move || {
            server::serve_listener_with(
                listener,
                LocalHost::new(),
                server::Services::with_machine(machine),
            )
        });
    }

    let writer = bridged(&sock, "writer");
    let watcher = bridged(&sock, "watcher");
    assert!(
        writer
            .peer_features
            .iter()
            .any(|f| f == feature::MACHINE_TREE)
    );
    // Make sure the watcher's subscription is up (its server thread subscribes
    // before answering its first request).
    watcher.control.call(ControlRequest::Ping).unwrap();

    let ws = match writer
        .control
        .call(ControlRequest::WorkspaceCreate {
            name: Some("shared".into()),
            workspace: None,
        })
        .unwrap()
    {
        ReplyOk::WorkspaceTree(ws) => *ws,
        other => panic!("{other:?}"),
    };
    let tab = match writer
        .control
        .call(ControlRequest::TabCreate {
            workspace: ws.id,
            at: None,
            pane: seed(3, "/srv"),
            tab: None,
        })
        .unwrap()
    {
        ReplyOk::TabTree(tab) => *tab,
        other => panic!("{other:?}"),
    };

    watcher.expect_delta(
        ws.id,
        |d| matches!(d, LayoutDelta::WorkspaceCreated { workspace } if workspace.id == ws.id),
    );
    watcher.expect_delta(
        ws.id,
        |d| matches!(d, LayoutDelta::TabCreated { tab: t, .. } if t.id == tab.id),
    );
    // The created tab became active, and the *change of active tab* is its own
    // delta — implicit activation must not be something a client re-derives.
    watcher.expect_delta(
        ws.id,
        |d| matches!(d, LayoutDelta::ActiveTabChanged { tab: t } if *t == tab.id),
    );
    assert_eq!(
        writer.delta_count(),
        0,
        "a client must not be pushed its own operation"
    );

    // …and the rule holds in the other direction.
    watcher
        .control
        .call(ControlRequest::TabRename {
            workspace: ws.id,
            tab: tab.id,
            name: Some("build".into()),
        })
        .unwrap();
    writer.expect_delta(
        ws.id,
        |d| matches!(d, LayoutDelta::TabRenamed { name: Some(n), .. } if n == "build"),
    );
    assert_eq!(watcher.delta_count(), 3, "still only the writer's own ops");
}

/// Takeover semantics on the new tree, with **no record store served at
/// all**: the attach verbs predate the tree, and their contract — newcomer
/// wins, the displaced session is told, a stale detach cannot evict the
/// usurper — must survive the record store's retirement.
#[test]
fn attachment_rides_the_tree_when_no_record_store_is_served() {
    use tty7_core::host::local::LocalHost;
    use tty7_core::host::server;

    let dir = data_dir();
    let machine = tty7_core::core::machine::MachineStore::open(machine_file(&dir));
    let sock = dir.path().join("control.sock");
    let listener = server::bind_control_socket(&sock).unwrap();
    {
        let machine = Arc::clone(&machine);
        std::thread::spawn(move || {
            server::serve_listener_with(
                listener,
                LocalHost::new(),
                server::Services::with_machine(machine),
            )
        });
    }
    let ws = machine
        .workspace_create(None, Some("shared".into()), None)
        .unwrap();

    let laptop = bridged(&sock, "laptop");
    let desktop = bridged(&sock, "desktop");

    let attach = |client: &Client| {
        client.control.call(ControlRequest::WorkspaceAttach {
            id: ws.id.to_string(),
        })
    };
    match attach(&laptop).expect("first attach") {
        ReplyOk::Attached { took_over_from } => assert_eq!(took_over_from, None),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        machine.attachment(ws.id).map(|a| a.hostname),
        Some("laptop".into()),
        "the tree's own record says who holds the workspace"
    );

    // The newcomer wins, learns whom it displaced, and the displaced session
    // is pushed a Preempted notice.
    match attach(&desktop).expect("takeover") {
        ReplyOk::Attached { took_over_from } => {
            assert_eq!(took_over_from.as_deref(), Some("laptop"));
        }
        other => panic!("{other:?}"),
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let seen = laptop.events.lock().unwrap().clone();
        if seen.iter().any(|e| {
            matches!(e, ControlEvent::Preempted { workspace, by }
                if *workspace == ws.id.to_string() && by == "desktop")
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "no Preempted push; saw {seen:?}");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The preempted session tidying up must not evict the usurper.
    laptop
        .control
        .call(ControlRequest::WorkspaceDetach {
            id: ws.id.to_string(),
        })
        .expect("a stale detach is success, not eviction");
    assert_eq!(
        machine.attachment(ws.id).map(|a| a.hostname),
        Some("desktop".into())
    );
}

/// A `--stdio --bridge` child connected to an already-listening control
/// socket — the two-hop shape a real multi-client machine has.
fn bridged(sock: &Path, token: &str) -> Client {
    let hello = ControlHello::host_rpc(token, token);
    let mut child = Command::new(env!("CARGO_BIN_EXE_tty7-server"))
        .args(["--stdio", "--bridge", "--control-sock"])
        .arg(sock)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("could not start the bridging client");
    let stdout = child.stdout.take().expect("piped");
    let stdin = child.stdin.take().expect("piped");
    let closer: Arc<dyn LinkShutdown> = Arc::new(ServerProcess {
        child: Mutex::new(Some(child)),
    });
    let events: Arc<Mutex<Vec<ControlEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let control = ControlClient::connect_with(
        stdout,
        stdin,
        Some(closer),
        &hello,
        Box::new(move |e| sink.lock().unwrap_or_else(|e| e.into_inner()).push(e)),
    )
    .expect("bridge handshake");
    let peer_features = control.hello().features.clone();
    Client {
        control,
        events,
        peer_features,
    }
}

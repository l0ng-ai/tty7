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

struct Client {
    control: ControlClient,
    events: Arc<Mutex<Vec<ControlEvent>>>,
    peer_features: Vec<String>,
}

impl Client {
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
        cwd: Some(cwd.to_string()),
        ..PaneSeed::bare(pane)
    }
}

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

#[test]
fn the_tree_is_built_by_operations_and_lives_in_the_servers_file() {
    let dir = data_dir();
    let client = connect(dir.path(), "ops");

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

    let text = std::fs::read_to_string(machine_file(&dir)).expect("the server wrote its tree");
    assert!(text.contains(&ws.id.to_string()), "{text}");

    let missing = client
        .control
        .call(ControlRequest::WorkspaceTree {
            workspace: WorkspaceId::new(),
        })
        .unwrap_err();
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);
}

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
    watcher.expect_delta(
        ws.id,
        |d| matches!(d, LayoutDelta::ActiveTabChanged { tab: t } if *t == tab.id),
    );
    assert_eq!(
        writer.delta_count(),
        0,
        "a client must not be pushed its own operation"
    );

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

    // And a peer asking for the tree is told the same. `tty7 ls` fills its
    // ATTACHED column from this answer, so an attachment that lived only in
    // the server's memory read to everyone else as "nobody is holding it".
    match desktop
        .control
        .call(ControlRequest::MachineGet)
        .expect("machine tree")
    {
        ReplyOk::MachineTree(m) => {
            let seen = m
                .workspaces
                .iter()
                .find(|w| w.id == ws.id)
                .expect("the shared workspace");
            let held = seen.attachment.as_ref().expect("held by the laptop");
            assert_eq!(held.hostname, "laptop");
            assert!(
                held.token.is_empty(),
                "the holder's token stays on the holder's connection"
            );
        }
        other => panic!("{other:?}"),
    }

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

/// Start a server the way an upgraded install starts, and let it exit.
///
/// `HOME` is a scratch directory of the test's own, which is the whole reason
/// this case can exist: the legacy path is derived from `HOME`, so a test that
/// borrowed the developer's would be reaching for their real `machine.json`.
/// stdin is closed, so the link is at EOF before it is read — the startup work
/// this test is about has already run by then, and the process leaves rather
/// than serving nothing.
fn started_once(home: &Path, extra: &[&str]) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tty7-server"))
        .args(["--stdio", "--serve"])
        .args(extra)
        .env("HOME", home)
        .env_remove("TTY7_DATA_DIR")
        .env_remove("TTY7_CONFIG_DIR")
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("could not start tty7-server --stdio --serve");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait().expect("waiting on tty7-server") {
            Some(_) => return,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("tty7-server kept running with its link at EOF");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn seed_legacy_tree(home: &Path) -> PathBuf {
    let legacy = home.join(".local").join("share").join("tty7");
    std::fs::create_dir_all(&legacy).unwrap();
    let file = legacy.join(MACHINE_FILE);
    std::fs::write(&file, br#"{"workspaces":[],"panes":[]}"#).unwrap();
    file
}

/// The upgrade, end to end: the daemon moves the tree the old build left in the
/// data directory into the config directory before it opens the store, so a
/// machine's workspaces survive the release that changed where they live.
#[test]
fn a_server_started_after_the_upgrade_carries_the_legacy_tree_in() {
    let home = tempfile::TempDir::new().unwrap();
    let legacy = seed_legacy_tree(home.path());

    started_once(home.path(), &[]);

    assert!(
        home.path().join(".config/tty7").join(MACHINE_FILE).exists(),
        "the tree must arrive beside the rest of the config directory"
    );
    assert!(
        !legacy.exists(),
        "a move leaves nothing to be adopted twice"
    );
}

/// And the half that keeps the upgrade from becoming the bug: a second tty7 on
/// a config directory of its own must not rename the machine's tree into it.
/// Whichever instance happened to start first would otherwise decide, and the
/// primary would come up owning nothing.
#[test]
fn a_config_dir_of_its_own_leaves_the_machines_tree_alone() {
    let home = tempfile::TempDir::new().unwrap();
    let other = tempfile::TempDir::new().unwrap();
    let legacy = seed_legacy_tree(home.path());

    started_once(
        home.path(),
        &["--config-dir", &other.path().to_string_lossy()],
    );

    assert!(
        legacy.exists(),
        "the machine's tree belongs to the instance running out of its config directory"
    );
    assert!(
        !other.path().join(MACHINE_FILE).exists(),
        "a second instance starts on an empty tree, not on somebody else's"
    );
}

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

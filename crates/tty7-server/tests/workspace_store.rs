//! The workspace store, end to end against a real `tty7-server` child process.
//!
//! Same shape and the same reasoning as [`stdio_conformance`]: the client is
//! the shipped `ControlClient`, the wire is the control dialect over real
//! pipes, and the server is the shipped binary keeping its records in a file it
//! owns. What this file adds is the half the conformance suite cannot reach —
//! the store is not a `Host` method, so no amount of `read_dir` parity proves
//! that `WorkspacePut` reached a disk or that another client heard about it.
//!
//! The three things worth a process boundary:
//!
//! | | Why an in-process socket pair would not do |
//! |---|---|
//! | The record is on **the server's** disk | The whole storage split is "the machine is the authority". A store in the test's own address space proves nothing about that |
//! | `workspace-store` is advertised only when served | The capability bit is built from what the *binary* wires up, and that wiring lives in `main.rs` |
//! | A change reaches the **other** connection | Two clients, one server process, one file — the configuration the user actually has when their laptop and their desktop are both connected |
//!
//! Every case gets its own `$TTY7_DATA_DIR`, so no case can be explained by
//! another's leftovers and nothing here can touch the developer's real
//! `~/.local/share/tty7/workspaces.json`.

// Unix-only, for the same reason as `stdio_conformance.rs`: the server under
// test is a `--stdio` child, and two of the cases stand up a control socket.
#![cfg(unix)]

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tty7_core::core::workspace_store::{STORE_FILE, WorkspaceStore};
use tty7_core::daemon::control::{
    ControlClient, ControlEvent, ControlHello, ControlRequest, LinkShutdown, ReplyOk, feature,
};
use tty7_core::host::local::LocalHost;
use tty7_core::host::server;

/// The child, and the only way to end it — see `stdio_conformance.rs` for why a
/// `LinkShutdown` is what reaps a process-backed link.
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

/// One connected client: the RPC channel, plus everything the server pushed to
/// it.
struct Client {
    control: ControlClient,
    events: Arc<Mutex<Vec<ControlEvent>>>,
    peer_features: Vec<String>,
}

impl Client {
    /// Wait for a `WorkspaceChanged` naming `id`, or fail saying what did
    /// arrive. Polled rather than blocked on a channel because the event and
    /// the reply that caused it race by construction.
    fn expect_changed(&self, id: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let seen = self
                .events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if seen
                .iter()
                .any(|e| matches!(e, ControlEvent::WorkspaceChanged { id: got } if got == id))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no WorkspaceChanged for {id}; saw {seen:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait for the takeover notice naming `workspace` and `by`.
    fn expect_preempted(&self, workspace: &str, by: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let seen = self
                .events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if seen.iter().any(|e| {
                matches!(e, ControlEvent::Preempted { workspace: w, by: b }
                    if w == workspace && b == by)
            }) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no Preempted for {workspace} by {by}; saw {seen:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn changed_count(&self) -> usize {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|e| matches!(e, ControlEvent::WorkspaceChanged { .. }))
            .count()
    }
}

/// Start a `tty7-server --stdio --serve` whose store lives in `data_dir`, and
/// connect a client to it.
///
/// `--serve` rather than letting the mode be probed: a developer running these
/// tests may well have a real `tty7-server --daemon` up, and bridging to *that*
/// would be testing their machine's state — and, here, writing to their real
/// workspace file.
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

fn store_file(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join(STORE_FILE)
}

fn record(id: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": name,
        "session": {"active": 0, "tabs": [
            {"pane": {"Leaf": {"cwd": "/home/me/proj", "pane_id": 11}},
             "sidebar_group": "/home/me/proj"}
        ]},
        "last_active": 1_753_600_000u64,
    })
}

fn json(reply: ReplyOk) -> serde_json::Value {
    match reply {
        ReplyOk::Json(v) => v,
        other => panic!("expected a Json reply, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------

/// The capability bit is the client's cue that asking is worth a round trip, so
/// it has to reflect what the shipped binary actually wired up.
#[test]
fn the_server_advertises_the_workspace_store() {
    let dir = data_dir();
    let client = connect(dir.path(), "cap");
    assert!(
        client
            .peer_features
            .iter()
            .any(|f| f == feature::WORKSPACE_STORE),
        "features were {:?}",
        client.peer_features
    );
}

/// **The milestone's proof for M5.** The four RPCs against a real server, and
/// the record ends up in a file that server owns — the storage split is not a
/// diagram, it is this file on that machine.
#[test]
fn records_survive_in_a_file_the_server_owns() {
    let dir = data_dir();
    let client = connect(dir.path(), "rpc");

    assert_eq!(
        json(client.control.call(ControlRequest::WorkspaceList).unwrap()),
        serde_json::json!([])
    );

    for (id, name) in [("w-api", "api"), ("w-web", "web")] {
        client
            .control
            .call(ControlRequest::WorkspacePut {
                id: id.to_string(),
                json: record(id, name),
            })
            .expect("put");
    }

    // The file is on this machine only because the "remote" is this machine;
    // the point is that the *test process* never wrote it. Reading it with
    // plain `std::fs` is how we know the bytes went out through a pipe and came
    // back as a syscall someone else made.
    let text = std::fs::read_to_string(store_file(&dir)).expect("the server wrote its store");
    assert!(text.contains("w-api"), "{text}");
    assert!(text.contains("w-web"), "{text}");

    // Get answers exactly what was put.
    let got = json(
        client
            .control
            .call(ControlRequest::WorkspaceGet {
                id: "w-api".to_string(),
            })
            .unwrap(),
    );
    assert_eq!(got, record("w-api", "api"));

    // List answers both, in the order they were written.
    let listed = json(client.control.call(ControlRequest::WorkspaceList).unwrap());
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["w-api", "w-web"]);

    // A missing id is an error the client can tell from an empty record.
    let missing = client
        .control
        .call(ControlRequest::WorkspaceGet {
            id: "not-a-workspace".to_string(),
        })
        .unwrap_err();
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);

    // Delete reaches the disk, and deleting again is still success.
    for _ in 0..2 {
        client
            .control
            .call(ControlRequest::WorkspaceDelete {
                id: "w-api".to_string(),
            })
            .expect("delete");
    }
    let text = std::fs::read_to_string(store_file(&dir)).unwrap();
    assert!(!text.contains("w-api"), "{text}");
    assert!(text.contains("w-web"), "{text}");
}

/// A second connection to the same server sees the first one's records — that
/// is what "换台电脑连过来要看到同一份" means once the machine is fixed and the
/// client is not.
#[test]
fn a_later_client_sees_what_an_earlier_one_wrote() {
    let dir = data_dir();
    {
        let first = connect(dir.path(), "first");
        first
            .control
            .call(ControlRequest::WorkspacePut {
                id: "w".to_string(),
                json: record("w", "api"),
            })
            .expect("put");
        first.control.close();
    }

    // A brand-new server process, reading the file the previous one left.
    let second = connect(dir.path(), "second");
    let got = json(
        second
            .control
            .call(ControlRequest::WorkspaceGet {
                id: "w".to_string(),
            })
            .unwrap(),
    );
    assert_eq!(got["name"], "api");
    assert_eq!(got["session"]["tabs"][0]["pane"]["Leaf"]["pane_id"], 11);
}

/// Two clients on one machine at once. A change by one has to reach the other,
/// and must not come back to its author.
///
/// The store lives behind a listener, as it does under `--daemon`, and both
/// clients reach it as `--stdio --bridge` children — the same two-hop shape
/// `cli.rs` uses, and the configuration a user has when their laptop and their
/// desktop are both connected. A store per connection would pass every other
/// test in this file and fail this one.
#[test]
fn a_change_from_one_client_reaches_the_other() {
    let dir = data_dir();
    let store = WorkspaceStore::open(store_file(&dir));
    let sock = dir.path().join("control.sock");
    let listener = server::bind_control_socket(&sock).unwrap();
    {
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            server::serve_listener_with(
                listener,
                LocalHost::new(),
                server::Services::with_workspaces(store),
            )
        });
    }

    let writer = bridged(&sock, "writer");
    let watcher = bridged(&sock, "watcher");
    assert!(
        writer
            .peer_features
            .iter()
            .any(|f| f == feature::WORKSPACE_STORE)
    );

    writer
        .control
        .call(ControlRequest::WorkspacePut {
            id: "shared".to_string(),
            json: record("shared", "api"),
        })
        .expect("put");

    watcher.expect_changed("shared");
    assert_eq!(
        writer.changed_count(),
        0,
        "a client must not be pushed its own change"
    );

    // The watcher is looking at the same store, not at a copy.
    let got = json(
        watcher
            .control
            .call(ControlRequest::WorkspaceGet {
                id: "shared".to_string(),
            })
            .unwrap(),
    );
    assert_eq!(got["name"], "api");

    // A delete is a change too — and the watcher's own delete comes back to the
    // writer, which is the same rule seen from the other side.
    watcher
        .control
        .call(ControlRequest::WorkspaceDelete {
            id: "shared".to_string(),
        })
        .expect("delete");
    writer.expect_changed("shared");
    assert_eq!(watcher.changed_count(), 1, "still only the writer's put");
    assert_eq!(store.len(), 0);
}

/// **The takeover, across two real processes.**
///
/// The same two-client shape as the change-notification test, and for the same
/// reason: a takeover is by definition something one connection does to
/// *another*, so an in-process registry with two handles into it would prove
/// only that the data structure works. What has to hold is that the notice
/// crosses a pipe into a different program and that the displaced link actually
/// closes.
///
/// D8 is the assertion in the middle: the newcomer holds the workspace
/// afterwards. Rejecting the second client would satisfy "only one at a time"
/// just as well and is the decision this test exists to rule out.
#[test]
fn a_later_client_takes_the_workspace_and_the_first_is_cut_off() {
    let dir = data_dir();
    let store = WorkspaceStore::open(store_file(&dir));
    let sock = dir.path().join("control.sock");
    let listener = server::bind_control_socket(&sock).unwrap();
    {
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            server::serve_listener_with(
                listener,
                LocalHost::new(),
                server::Services::with_workspaces(store),
            )
        });
    }

    let laptop = bridged_for(&sock, "tok-laptop", "laptop", Some("w"));
    // The attach runs on the server thread after the handshake reply, so the
    // record is what says it happened — not the fact that we got a `HELLO_OK`.
    await_attachment(&store, "w", "laptop");
    assert!(laptop.control.call(ControlRequest::Ping).is_ok());

    let desktop = bridged_for(&sock, "tok-desktop", "desktop", Some("w"));

    // The displaced client is told which workspace it lost and to whom.
    laptop.expect_preempted("w", "desktop");
    // …and then its link is closed, because this connection existed for that
    // workspace: the server closes its stream.
    let deadline = Instant::now() + Duration::from_secs(10);
    while laptop.control.is_connected() {
        assert!(
            Instant::now() < deadline,
            "the displaced session's link stayed open"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        laptop
            .control
            .call(ControlRequest::Ping)
            .unwrap_err()
            .kind(),
        io::ErrorKind::ConnectionReset
    );

    // D8: the newcomer is the one holding it, and it can still work.
    await_attachment(&store, "w", "desktop");
    assert!(desktop.control.call(ControlRequest::Ping).is_ok());
    assert_eq!(
        desktop.changed_count(),
        0,
        "taking over is not a workspace change"
    );

    // Taking it back is the same operation in the other direction — that is all
    // the [Take Back] button is.
    let back = bridged_for(&sock, "tok-laptop-2", "laptop", None);
    let reply = back
        .control
        .call(ControlRequest::WorkspaceAttach { id: "w".into() })
        .expect("attach");
    assert_eq!(
        reply,
        ReplyOk::Attached {
            took_over_from: Some("desktop".to_string())
        }
    );
    desktop.expect_preempted("w", "laptop");
    await_attachment(&store, "w", "laptop");

    // The link that just did the taking was not opened *for* the workspace, so
    // it is a plain machine connection and keeps working — that is the shape the
    // GUI has, one link per machine.
    assert!(back.control.is_connected());
    assert!(back.control.call(ControlRequest::Ping).is_ok());
}

/// Poll until `hostname` holds `workspace`, or fail saying who does.
fn await_attachment(store: &Arc<WorkspaceStore>, workspace: &str, hostname: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let who = store.attachment(workspace);
        if who.as_ref().map(|a| a.hostname.as_str()) == Some(hostname) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{workspace} is held by {who:?}, not {hostname}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A `--stdio --bridge` child connected to an already-listening control socket.
fn bridged(sock: &Path, token: &str) -> Client {
    bridged_for(sock, token, "test-client", None)
}

/// [`bridged`], naming the client machine and, optionally, the workspace this
/// connection is opened *for* — the hello field the takeover keys on.
fn bridged_for(sock: &Path, token: &str, hostname: &str, workspace: Option<&str>) -> Client {
    let hello = ControlHello {
        control_version: tty7_core::daemon::control::CONTROL_VERSION,
        workspace: workspace.map(str::to_string),
        client_token: token.to_string(),
        client_hostname: hostname.to_string(),
    };
    bridged_with(sock, hello)
}

fn bridged_with(sock: &Path, hello: ControlHello) -> Client {
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

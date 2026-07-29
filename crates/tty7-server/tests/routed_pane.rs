//! **A remote workspace's pane, end to end, with no sshd and no network.**
//!
//! `remote_router.rs` proves the *control* dialect crosses the router; this file
//! proves the other one — the pane protocol — which is the half a remote
//! workspace needs before it can run anything at all. Until it existed a remote
//! window opened, listed files, and could not spawn a terminal.
//!
//! ## What stands in for what
//!
//! | Real thing | Here |
//! |---|---|
//! | The GUI's `RemoteTerminal` | a `UnixStream` speaking `ClientMsg`/`DaemonMsg` |
//! | The user's local daemon | a `UnixListener` + `RemoteRouter::route` |
//! | The SSH channel | `RouteTarget::LocalStdio` → a child process |
//! | The remote `tty7-server --daemon` | `tty7-server --stdio --pane` bridging to one |
//!
//! Only the middle hop is faked, and it is faked with the same
//! `RemoteRouter::route` the daemon calls. Everything on the far side is the
//! real binary: a real `--daemon` process, a real PTY, a real shell.
//!
//! ## Why `--config-dir` per test
//!
//! The "remote" pane daemon this stands up is a *real* daemon on this machine.
//! Pointing it at a temp config dir gives it its own socket, so it can neither
//! see nor be seen by the developer's own tty7 — and `Shutdown` at the end of
//! each test reaps it rather than leaving one per CI run.

// Unix-only for the same reason `remote_router.rs` is: the hop being tested is a
// Unix-domain socket, and `--stdio` is a Unix path by construction.
#![cfg(unix)]

use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use tty7_core::daemon::protocol::{ClientMsg, DaemonMsg, ShellSpec, WinSize};
use tty7_core::daemon::router::{RemoteRouter, RouteChannel, RouteHeader, negotiate};

const EXE: &str = env!("CARGO_BIN_EXE_tty7-server");

/// How long a test waits for a shell to say something. Generous: a cold daemon
/// launch plus a shell start on a loaded CI box is not instant, and a flaky
/// timeout here would read as a routing bug.
const OUTPUT_TIMEOUT: Duration = Duration::from_secs(30);

fn win() -> WinSize {
    WinSize {
        cols: 80,
        rows: 24,
        cell_w: 8,
        cell_h: 17,
    }
}

/// A shell with no startup files, so what comes back is the command's output and
/// not somebody's prompt theme.
fn plain_shell() -> ShellSpec {
    ShellSpec {
        program: "/bin/sh".to_string(),
        args: Vec::new(),
        args_are_tty7_defaults: false,
    }
}

/// Stand up the local hop: a socket that routes one connection and then returns.
///
/// One connection per hub, because that is exactly what the GUI does — a pane is
/// a connection, and `handle_conn` hands each one to the router separately.
fn hub(dir: &Path, name: &str) -> (std::path::PathBuf, std::thread::JoinHandle<()>) {
    let path = dir.join(name);
    let listener = UnixListener::bind(&path).unwrap();
    let thread = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = stream.try_clone().unwrap();
        let (kind, payload) = tty7_core::daemon::protocol::read_frame(&mut reader).unwrap();
        assert_eq!(kind, tty7_core::daemon::router::ROUTE_KIND);
        let header = RouteHeader::decode(&payload).unwrap();
        // The far end outliving the near one is normal (the client hangs up
        // first), so a closed pipe here is not a failure.
        let _ = RemoteRouter::route(stream, &header);
    });
    (path, thread)
}

/// The header a pane of a remote workspace writes, with this machine standing in
/// for the remote.
fn pane_header(config_dir: &Path) -> RouteHeader {
    RouteHeader::local_stdio(
        EXE,
        &[
            "--stdio",
            "--pane",
            "--config-dir",
            &config_dir.to_string_lossy(),
        ],
    )
    .for_pane()
}

/// Open a routed pane connection through a fresh hub.
fn routed(dir: &Path, name: &str, config_dir: &Path) -> (UnixStream, std::thread::JoinHandle<()>) {
    let (path, thread) = hub(dir, name);
    let mut sock = UnixStream::connect(&path).unwrap();
    let ack = negotiate(&mut sock, &pane_header(config_dir)).expect("the route should be accepted");
    assert_eq!(ack.link.as_deref(), Some("local-stdio"));
    (sock, thread)
}

/// Read frames until `needle` shows up in the accumulated PTY bytes.
///
/// Accumulating rather than matching per frame is the point: a PTY splits output
/// wherever it likes, and a test that expected one frame per line would pass or
/// fail on scheduling.
fn read_until(sock: &mut UnixStream, needle: &str) -> String {
    let deadline = Instant::now() + OUTPUT_TIMEOUT;
    let mut seen = String::new();
    sock.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    while Instant::now() < deadline {
        match DaemonMsg::read(sock) {
            Ok(DaemonMsg::Output(bytes)) | Ok(DaemonMsg::Snapshot(bytes)) => {
                seen.push_str(&String::from_utf8_lossy(&bytes));
                if seen.contains(needle) {
                    return seen;
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => panic!("stream died waiting for {needle:?}: {e}\nsaw: {seen:?}"),
        }
    }
    panic!("timed out waiting for {needle:?}\nsaw: {seen:?}");
}

/// Stop the daemon this test started, so it does not outlive the run.
fn shutdown(dir: &Path, config_dir: &Path) {
    let (path, thread) = hub(dir, "shutdown.sock");
    if let Ok(mut sock) = UnixStream::connect(&path)
        && negotiate(&mut sock, &pane_header(config_dir)).is_ok()
    {
        let _ = ClientMsg::Shutdown.encode(&mut sock);
        // The daemon exits without replying, so read to EOF rather than
        // expecting a frame.
        let _ = sock.read(&mut [0u8; 64]);
    }
    let _ = thread.join();
}

/// **The milestone's proof.** Open a pane on the "remote", type at it, see what
/// it printed, hang up, come back, and find the pane still there with its
/// scrollback.
///
/// Every claim a remote workspace makes is in this one test: the pane exists on
/// the far machine (it survives the connection that made it), the hot path
/// crosses the router intact in both directions, and reattach finds the same
/// pane rather than a new one.
#[test]
fn a_routed_pane_spawns_takes_input_and_survives_a_reconnect() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = dir.path().join("remote-config");
    std::fs::create_dir_all(&config).unwrap();

    // ---- connect, spawn ---------------------------------------------------
    let (mut sock, hub_thread) = routed(dir.path(), "pane-1.sock", &config);
    ClientMsg::Spawn {
        cwd: Some(dir.path().to_path_buf()),
        size: win(),
        shell: Some(plain_shell()),
        owner: None,
    }
    .encode(&mut sock)
    .unwrap();

    let pane_id = match DaemonMsg::read(&mut sock).unwrap() {
        DaemonMsg::Spawned { pane_id } => pane_id,
        other => panic!("expected Spawned through the router, got {other:?}"),
    };

    // ---- input → output ---------------------------------------------------
    // A marker no shell prompt would produce on its own, echoed by a command
    // that exists in every POSIX shell.
    ClientMsg::Input(b"echo rou''ted-pane-alive\n".to_vec())
        .encode(&mut sock)
        .unwrap();
    read_until(&mut sock, "routed-pane-alive");

    // ---- disconnect -------------------------------------------------------
    // `Detach`, not `Kill`: the pane is meant to keep running on the far side,
    // which is the entire proposition of a remote workspace.
    ClientMsg::Detach.encode(&mut sock).unwrap();
    drop(sock);
    let _ = hub_thread.join();

    // ---- reconnect --------------------------------------------------------
    let (mut back, hub_thread) = routed(dir.path(), "pane-2.sock", &config);
    ClientMsg::Attach {
        pane_id,
        size: win(),
    }
    .encode(&mut back)
    .unwrap();

    // The snapshot replays the ring the *remote* daemon kept, so the marker
    // printed before the disconnect is still there.
    read_until(&mut back, "routed-pane-alive");

    // And it is live, not just a recording.
    ClientMsg::Input(b"echo st''ill-here\n".to_vec())
        .encode(&mut back)
        .unwrap();
    read_until(&mut back, "still-here");

    drop(back);
    let _ = hub_thread.join();
    shutdown(dir.path(), &config);
}

/// The pane channel and the control channel are **not** interchangeable.
///
/// A header that forgets `for_pane()` reaches the control socket, where a
/// `Spawn` is an unknown frame. This is what "the window opens but nothing runs
/// in it" looked like, so it is pinned rather than left to the reader.
#[test]
fn the_channel_decides_which_dialect_the_route_carries() {
    let control = RouteHeader::local_stdio(EXE, &["--stdio"]);
    assert_eq!(control.channel, RouteChannel::Control);
    assert_eq!(control.clone().for_pane().channel, RouteChannel::Pane);

    // The wire tag is what a *different* build matches on, so it is pinned
    // rather than left to the variant name — and the default has to keep
    // decoding as `control`, because that is what every header written before
    // the field existed meant.
    let mut buf = Vec::new();
    control.clone().for_pane().write(&mut buf).unwrap();
    let (_, payload) = tty7_core::daemon::protocol::read_frame(&mut buf.as_slice()).unwrap();
    let json = String::from_utf8(payload).unwrap();
    assert!(json.contains(r#""channel":"pane""#), "{json}");

    let legacy = r#"{"target":{"local_stdio":{"program":"x","args":[]}}}"#;
    let decoded = RouteHeader::decode(legacy.as_bytes()).unwrap();
    assert_eq!(decoded.channel, RouteChannel::Control);
}

/// A routed pane's `Kill` reaches the machine the pane is on.
///
/// Pane ids are per-daemon, so this is not a convenience: an unrouted `Kill`
/// does not fail, it succeeds against whatever local pane happens to hold the
/// same number.
#[test]
fn a_routed_kill_reaches_the_pane_it_names() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = dir.path().join("remote-config");
    std::fs::create_dir_all(&config).unwrap();

    let (mut sock, hub_thread) = routed(dir.path(), "pane-1.sock", &config);
    ClientMsg::Spawn {
        cwd: Some(dir.path().to_path_buf()),
        size: win(),
        shell: Some(plain_shell()),
        owner: None,
    }
    .encode(&mut sock)
    .unwrap();
    let pane_id = match DaemonMsg::read(&mut sock).unwrap() {
        DaemonMsg::Spawned { pane_id } => pane_id,
        other => panic!("expected Spawned, got {other:?}"),
    };
    ClientMsg::Detach.encode(&mut sock).unwrap();
    drop(sock);
    let _ = hub_thread.join();

    // It is on the remote's registry...
    let (mut list, hub_thread) = routed(dir.path(), "list-1.sock", &config);
    ClientMsg::List.encode(&mut list).unwrap();
    let before = match DaemonMsg::read(&mut list).unwrap() {
        DaemonMsg::PaneList(panes) => panes,
        other => panic!("expected PaneList, got {other:?}"),
    };
    assert!(before.iter().any(|p| p.pane_id == pane_id), "{before:?}");
    drop(list);
    let _ = hub_thread.join();

    // ...and a routed Kill takes it off.
    let (mut kill, hub_thread) = routed(dir.path(), "kill-1.sock", &config);
    ClientMsg::Kill { pane_id }.encode(&mut kill).unwrap();
    let _ = kill.shutdown(std::net::Shutdown::Write);
    drop(kill);
    let _ = hub_thread.join();

    let (mut list, hub_thread) = routed(dir.path(), "list-2.sock", &config);
    ClientMsg::List.encode(&mut list).unwrap();
    let after = match DaemonMsg::read(&mut list).unwrap() {
        DaemonMsg::PaneList(panes) => panes,
        other => panic!("expected PaneList, got {other:?}"),
    };
    assert!(!after.iter().any(|p| p.pane_id == pane_id), "{after:?}");
    drop(list);
    let _ = hub_thread.join();

    shutdown(dir.path(), &config);
}

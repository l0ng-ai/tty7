#![cfg(unix)]

use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use tty7_core::daemon::protocol::{ClientMsg, DaemonMsg, ShellSpec, WinSize};
use tty7_core::daemon::router::{RemoteRouter, RouteChannel, RouteHeader, negotiate};

const EXE: &str = env!("CARGO_BIN_EXE_tty7-server");

const OUTPUT_TIMEOUT: Duration = Duration::from_secs(30);

fn win() -> WinSize {
    WinSize {
        cols: 80,
        rows: 24,
        cell_w: 8,
        cell_h: 17,
    }
}

fn plain_shell() -> ShellSpec {
    ShellSpec {
        program: "/bin/sh".to_string(),
        args: Vec::new(),
        args_are_tty7_defaults: false,
    }
}

fn hub(dir: &Path, name: &str) -> (std::path::PathBuf, std::thread::JoinHandle<()>) {
    let path = dir.join(name);
    let listener = UnixListener::bind(&path).unwrap();
    let thread = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = stream.try_clone().unwrap();
        let (kind, payload) = tty7_core::daemon::protocol::read_frame(&mut reader).unwrap();
        assert_eq!(kind, tty7_core::daemon::router::ROUTE_KIND);
        let header = RouteHeader::decode(&payload).unwrap();
        let _ = RemoteRouter::route(stream, &header);
    });
    (path, thread)
}

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

fn routed(dir: &Path, name: &str, config_dir: &Path) -> (UnixStream, std::thread::JoinHandle<()>) {
    let (path, thread) = hub(dir, name);
    let mut sock = UnixStream::connect(&path).unwrap();
    let ack = negotiate(&mut sock, &pane_header(config_dir)).expect("the route should be accepted");
    assert_eq!(ack.link.as_deref(), Some("local-stdio"));
    (sock, thread)
}

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

fn shutdown(dir: &Path, config_dir: &Path) {
    let (path, thread) = hub(dir, "shutdown.sock");
    if let Ok(mut sock) = UnixStream::connect(&path)
        && negotiate(&mut sock, &pane_header(config_dir)).is_ok()
    {
        let _ = ClientMsg::Shutdown.encode(&mut sock);
        let _ = sock.read(&mut [0u8; 64]);
    }
    let _ = thread.join();
}

#[test]
fn a_routed_pane_spawns_takes_input_and_survives_a_reconnect() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = dir.path().join("remote-config");
    std::fs::create_dir_all(&config).unwrap();

    let (mut sock, hub_thread) = routed(dir.path(), "pane-1.sock", &config);
    ClientMsg::Spawn {
        cwd: Some(dir.path().to_path_buf()),
        size: win(),
        shell: Some(plain_shell()),
        owner: None,
        workspace: None,
        restore: None,
    }
    .encode(&mut sock)
    .unwrap();

    let pane_id = match DaemonMsg::read(&mut sock).unwrap() {
        DaemonMsg::Spawned { pane_id } => pane_id,
        other => panic!("expected Spawned through the router, got {other:?}"),
    };

    ClientMsg::Input(b"echo rou''ted-pane-alive\n".to_vec())
        .encode(&mut sock)
        .unwrap();
    read_until(&mut sock, "routed-pane-alive");

    ClientMsg::Detach.encode(&mut sock).unwrap();
    drop(sock);
    let _ = hub_thread.join();

    let (mut back, hub_thread) = routed(dir.path(), "pane-2.sock", &config);
    ClientMsg::Attach {
        pane_id,
        size: win(),
    }
    .encode(&mut back)
    .unwrap();

    read_until(&mut back, "routed-pane-alive");

    ClientMsg::Input(b"echo st''ill-here\n".to_vec())
        .encode(&mut back)
        .unwrap();
    read_until(&mut back, "still-here");

    drop(back);
    let _ = hub_thread.join();
    shutdown(dir.path(), &config);
}

#[test]
fn the_channel_decides_which_dialect_the_route_carries() {
    let control = RouteHeader::local_stdio(EXE, &["--stdio"]);
    assert_eq!(control.channel, RouteChannel::Control);
    assert_eq!(control.clone().for_pane().channel, RouteChannel::Pane);

    let mut buf = Vec::new();
    control.clone().for_pane().write(&mut buf).unwrap();
    let (_, payload) = tty7_core::daemon::protocol::read_frame(&mut buf.as_slice()).unwrap();
    let json = String::from_utf8(payload).unwrap();
    assert!(json.contains(r#""channel":"pane""#), "{json}");

    let legacy = r#"{"target":{"local_stdio":{"program":"x","args":[]}}}"#;
    let decoded = RouteHeader::decode(legacy.as_bytes()).unwrap();
    assert_eq!(decoded.channel, RouteChannel::Control);
}

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
        workspace: None,
        restore: None,
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

    let (mut list, hub_thread) = routed(dir.path(), "list-1.sock", &config);
    ClientMsg::List.encode(&mut list).unwrap();
    let before = match DaemonMsg::read(&mut list).unwrap() {
        DaemonMsg::PaneList(panes) => panes,
        other => panic!("expected PaneList, got {other:?}"),
    };
    assert!(before.iter().any(|p| p.pane_id == pane_id), "{before:?}");
    drop(list);
    let _ = hub_thread.join();

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

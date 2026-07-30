#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Command, Stdio};

use tty7_core::daemon::control::ControlHello;
use tty7_core::daemon::remote_link::{RemoteEnv, remote_control_socket};
use tty7_core::daemon::router::{RemoteRouter, RouteAck, RouteHeader};
use tty7_core::host::Host;
use tty7_core::host::remote::RemoteHost;

const EXE: &str = env!("CARGO_BIN_EXE_tty7-server");

#[test]
fn a_routed_connection_reaches_a_real_server() {
    let dir = tempfile::TempDir::new().unwrap();
    let hub = dir.path().join("hub.sock");
    let listener = UnixListener::bind(&hub).unwrap();

    let router = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = stream.try_clone().unwrap();
        let (kind, payload) = tty7_core::daemon::protocol::read_frame(&mut reader).unwrap();
        assert_eq!(kind, tty7_core::daemon::router::ROUTE_KIND);
        let header = RouteHeader::decode(&payload).unwrap();
        RemoteRouter::route(stream, &header)
    });

    let mut sock = UnixStream::connect(&hub).unwrap();
    let missing = dir.path().join("nobody-here.sock");
    let header = RouteHeader::local_stdio(
        EXE,
        &[
            "--stdio",
            "--serve",
            "--control-sock",
            &missing.to_string_lossy(),
        ],
    );
    header.write(&mut sock).unwrap();

    let ack = RouteAck::read(&mut sock).expect("the route should be accepted");
    assert_eq!(ack.link.as_deref(), Some("local-stdio"));

    let hello = ControlHello::host_rpc("router-test", "localhost");
    let host = RemoteHost::over_unix(sock, "routed:local-stdio", &hello)
        .expect("handshake through the router");

    let sandbox = tempfile::TempDir::new().unwrap();
    let file = host.join(sandbox.path(), "through-the-router.txt");
    host.write_file(&file, b"two hops and a pipe").unwrap();
    assert_eq!(host.read_file(&file, 1024).unwrap(), b"two hops and a pipe");

    let big = host.join(sandbox.path(), "big.bin");
    let body: Vec<u8> = (0..2 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    host.write_file(&big, &body).unwrap();
    assert!(host.read_file(&big, 8 * 1024 * 1024).unwrap() == body);

    assert_eq!(host.read_dir(sandbox.path(), None).unwrap().len(), 2);

    drop(host);
    let _ = router.join().unwrap();
}

#[test]
fn an_unreachable_target_is_reported_not_dropped() {
    let dir = tempfile::TempDir::new().unwrap();
    let hub = dir.path().join("hub.sock");
    let listener = UnixListener::bind(&hub).unwrap();

    let router = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = stream.try_clone().unwrap();
        let (_, payload) = tty7_core::daemon::protocol::read_frame(&mut reader).unwrap();
        let header = RouteHeader::decode(&payload).unwrap();
        RemoteRouter::route(stream, &header)
    });

    let mut sock = UnixStream::connect(&hub).unwrap();
    RouteHeader::local_stdio("tty7-server-that-does-not-exist", &[])
        .write(&mut sock)
        .unwrap();

    let err = RouteAck::read(&mut sock).expect_err("there is nothing to route to");
    assert!(
        !err.to_string().is_empty(),
        "the failure must say something"
    );
    assert!(router.join().unwrap().is_err());
}

#[test]
fn the_derived_remote_socket_is_the_one_the_server_binds() {
    let with_runtime = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let runtime_path = with_runtime.path().to_string_lossy().to_string();
    let home_path = home.path().to_string_lossy().to_string();

    let bound = bound_control_socket(Some(&runtime_path), &home_path);
    let derived = remote_control_socket(&RemoteEnv {
        control_sock: None,
        xdg_runtime_dir: Some(runtime_path.clone()),
        home: Some(home_path.clone()),
        tmpdir: std::env::var("TMPDIR").ok(),
    });
    assert_eq!(derived.as_deref(), Some(bound.as_str()));

    let bound = bound_control_socket(None, &home_path);
    let derived = remote_control_socket(&RemoteEnv {
        control_sock: None,
        xdg_runtime_dir: None,
        home: Some(home_path.clone()),
        tmpdir: std::env::var("TMPDIR").ok(),
    });
    assert_eq!(derived.as_deref(), Some(bound.as_str()));
}

fn bound_control_socket(runtime_dir: Option<&str>, home: &str) -> String {
    let config = tempfile::TempDir::new().unwrap();
    let mut cmd = Command::new(EXE);
    cmd.arg("--daemon")
        .arg("--config-dir")
        .arg(config.path())
        .env("HOME", home)
        .env_remove("TTY7_CONTROL_SOCK")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    match runtime_dir {
        Some(dir) => cmd.env("XDG_RUNTIME_DIR", dir),
        None => cmd.env_remove("XDG_RUNTIME_DIR"),
    };
    let mut child = cmd.spawn().expect("start tty7-server --daemon");

    let stderr = BufReader::new(child.stderr.take().expect("piped"));
    let mut bound = None;
    for line in stderr.lines().map_while(Result::ok) {
        if let Some(path) = line.strip_prefix("tty7-server: control socket at ") {
            bound = Some(path.to_string());
            break;
        }
        assert!(
            !line.contains("control listener unavailable"),
            "the server could not bind at all: {line}"
        );
    }
    let _ = child.kill();
    let _ = child.wait();
    bound.expect("the server prints the control socket it bound")
}

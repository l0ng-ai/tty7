#![cfg(unix)]

use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use tty7_core::daemon::control::{ControlHello, LinkShutdown};
use tty7_core::host::SharedHost;
use tty7_core::host::conformance::Sandbox;
use tty7_core::host::remote::RemoteHost;

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

struct TempSandbox(tempfile::TempDir);

impl Sandbox for TempSandbox {
    fn path(&self) -> &Path {
        self.0.path()
    }

    fn symlink(&self, target: &Path, link: &Path) -> Option<io::Result<()>> {
        #[cfg(unix)]
        {
            Some(std::os::unix::fs::symlink(target, link))
        }
        #[cfg(not(unix))]
        {
            let _ = (target, link);
            None
        }
    }
}

fn stdio_host() -> (SharedHost, TempSandbox) {
    let sandbox = TempSandbox(tempfile::TempDir::new().unwrap());
    let mut child = Command::new(env!("CARGO_BIN_EXE_tty7-server"))
        .args(["--stdio", "--serve"])
        .env("TTY7_DATA_DIR", sandbox.path())
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

    let hello = ControlHello::host_rpc("stdio-conformance", "localhost");
    let host = RemoteHost::connect_with(stdout, stdin, Some(closer), "stdio:conformance", &hello)
        .expect("handshake with tty7-server --stdio");

    (host.into_shared(), sandbox)
}

tty7_core::host_conformance_suite!(remote_stdio, stdio_host);

#[test]
fn the_whole_suite_ran_against_the_server() {
    let names: Vec<&str> = tty7_core::host::conformance::CASES
        .iter()
        .map(|(n, _)| *n)
        .collect();
    assert!(
        names.len() >= 46,
        "the conformance suite shrank to {} cases: {names:?}",
        names.len()
    );
}

#[test]
fn the_server_really_is_another_process() {
    let (host, sandbox) = stdio_host();
    let marker = host.join(sandbox.path(), "written-over-the-wire.txt");
    host.write_file(&marker, b"from the client").unwrap();

    assert_eq!(std::fs::read(&marker).unwrap(), b"from the client");

    let from_here = sandbox.path().join("written-locally.txt");
    std::fs::write(&from_here, b"from the test").unwrap();
    assert_eq!(
        host.read_file(&from_here, 1024).unwrap(),
        b"from the test",
        "the server read a file this process wrote"
    );
    assert!(host.is_connected());
}

#[test]
fn dropping_the_host_reaps_the_server() {
    let (host, sandbox) = stdio_host();
    assert!(host.exists(sandbox.path()));
    drop(host);
}

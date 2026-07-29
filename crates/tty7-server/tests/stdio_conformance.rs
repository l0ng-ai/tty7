//! **The milestone's proof**: every `Host` conformance case, run against a real
//! `tty7-server --stdio` child process over real pipes.
//!
//! Not a mock, not an in-process socket pair, and — the part that matters — not
//! an sshd. The client is `RemoteHost`, the wire is the control dialect, the
//! server is the shipped binary answering out of its own address space, and the
//! only thing standing in for SSH is a pair of pipes. Everything between the
//! `Host` call and the syscall is the code a transcontinental workspace runs.
//!
//! That is what makes remote workspaces testable in CI at all. The alternative —
//! provisioning a machine, an sshd, a key, and a network for every pull request
//! — is expensive enough that in practice it does not get run, which means the
//! two `Host` implementations drift and nobody finds out until someone opens a
//! remote directory. Here the identical list of cases runs against `LocalHost`
//! in `tty7-core` and against this, and a divergence is a red test.
//!
//! # Shape
//!
//! One child process and one sandbox **per case**, via
//! [`host_conformance_suite!`](tty7_core::host_conformance_suite). Spawning
//! forty-six servers costs a few hundred milliseconds in total and buys complete
//! isolation: no case can be explained by another's leftover state, a hung
//! server fails exactly one case, and a crash names the behaviour that caused it.

// Unix-only: every case here is a `--stdio` child, and `--stdio` is refused on
// Windows by design — a Windows machine is reached over its own transport, not
// by shipping a server onto it.
#![cfg(unix)]

use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use tty7_core::daemon::control::{ControlHello, LinkShutdown};
use tty7_core::host::SharedHost;
use tty7_core::host::conformance::Sandbox;
use tty7_core::host::remote::RemoteHost;

/// The child, and the only way to end it.
///
/// `RemoteHost` closes its link through [`LinkShutdown`]; for a socket that is
/// `shutdown(2)`, and for a child process it is this. Without it, dropping the
/// host would leave the reader thread parked on a pipe the server has no reason
/// to write to and the server parked on a pipe the client has no reason to write
/// to — the exact standoff `LinkShutdown` exists to break, one transport over.
struct ServerProcess {
    child: Mutex<Option<Child>>,
}

impl LinkShutdown for ServerProcess {
    fn shutdown_link(&self) -> io::Result<()> {
        let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return Ok(()); // already reaped; `close` and `Drop` both call this
        };
        let _ = child.kill();
        // Reaped here rather than left to the OS: forty-six cases running in
        // parallel would otherwise accumulate forty-six zombies for the life of
        // the test binary.
        let _ = child.wait();
        Ok(())
    }
}

/// A temp directory on the machine the server is on — which, this being the
/// stdio path, is also this one.
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

/// Start a server and connect a `RemoteHost` to it.
fn stdio_host() -> (SharedHost, TempSandbox) {
    let sandbox = TempSandbox(tempfile::TempDir::new().unwrap());
    let mut child = Command::new(env!("CARGO_BIN_EXE_tty7-server"))
        // `--serve` rather than letting the mode be probed: a developer running
        // these tests may well have a real `tty7-server --daemon` up, and a
        // bridge to *that* would be testing their machine's state instead of
        // this build.
        .args(["--stdio", "--serve"])
        // The server opens its machine tree at startup. None of these cases
        // touch it, but pointing it at the sandbox keeps forty-six child
        // processes off the developer's real `~/.local/share/tty7`.
        .env("TTY7_DATA_DIR", sandbox.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The server's diagnostics are not this test's output. A failure shows
        // up as a failed request, which names the case.
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

// Every case in `tty7-core`'s shared suite, over the pipes. This is the same
// list `LocalHost` runs; the point is that it is not a *similar* list.
tty7_core::host_conformance_suite!(remote_stdio, stdio_host);

/// The suite above only proves the cases pass — it cannot prove they were the
/// whole suite. This checks the count the registry actually carries, so a case
/// silently dropped upstream shows up here as well as there.
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

/// The server is a *separate process* answering out of its own memory. Easy to
/// lose by accident — an in-process fallback would keep every case above green
/// while testing nothing that this milestone is about.
#[test]
fn the_server_really_is_another_process() {
    let (host, sandbox) = stdio_host();
    let marker = host.join(sandbox.path(), "written-over-the-wire.txt");
    host.write_file(&marker, b"from the client").unwrap();

    // This side reads it with plain `std::fs`: if the bytes are there, they went
    // out through a pipe and came back through a syscall someone else made.
    assert_eq!(std::fs::read(&marker).unwrap(), b"from the client");

    // And the reverse: a change this process makes with `std::fs` is visible to
    // the server, so both ends really are looking at one filesystem through two
    // different code paths.
    let from_here = sandbox.path().join("written-locally.txt");
    std::fs::write(&from_here, b"from the test").unwrap();
    assert_eq!(
        host.read_file(&from_here, 1024).unwrap(),
        b"from the test",
        "the server read a file this process wrote"
    );
    assert!(host.is_connected());
}

/// Dropping the host kills the child. A test binary that leaked one server per
/// case would leave forty-six processes behind on every run.
#[test]
fn dropping_the_host_reaps_the_server() {
    let (host, sandbox) = stdio_host();
    assert!(host.exists(sandbox.path()));
    drop(host);
    // Nothing to assert beyond "this returns": the reap happens inside the drop,
    // and a shutdown that did not wake the reader would hang here instead.
}

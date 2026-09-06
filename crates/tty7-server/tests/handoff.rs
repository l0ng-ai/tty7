//! The daemon replaces its own binary and the shells never notice.
//!
//! Everything else about a handoff can be checked in a unit test — the blob
//! round-trips, the flags parse, the ids keep climbing. None of that answers
//! the only question that matters, which is whether the process on the other
//! end of the pty is still the same process afterwards. So this runs a real
//! daemon, puts a real shell in a real pty, and asks the shell.
//!
//! The proof is a variable. `tty7_kept=…` lives in that shell's memory and
//! nowhere else: no file has it, the daemon has never seen it, and a shell
//! started after the handoff would answer with an empty string. Getting the
//! value back is something only the original process can do.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tty7_core::client::PaneClient;
use tty7_core::daemon::protocol::{DaemonMsg, ShellSpec, WinSize};

const READY_WITHIN: Duration = Duration::from_secs(30);
const STREAM_WITHIN: Duration = Duration::from_secs(30);

struct Daemon {
    child: Child,
    dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Daemon {
        let dir = tempfile::TempDir::new().unwrap();
        let child = Command::new(Self::binary())
            .arg("--daemon")
            .arg("--config-dir")
            .arg(dir.path())
            .env("TTY7_DATA_DIR", dir.path())
            .env("TTY7_CONTROL_SOCK", dir.path().join("control.sock"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start tty7-server --daemon");
        let daemon = Daemon { child, dir };
        daemon.await_ready();
        daemon
    }

    fn binary() -> PathBuf {
        PathBuf::from(env!("CARGO_BIN_EXE_tty7-server"))
    }

    fn panes(&self) -> PaneClient {
        PaneClient::at(self.dir.path().join("daemon.sock"))
    }

    /// The pid the daemon records for itself. An `exec` keeps it; stopping and
    /// starting cannot.
    fn recorded_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.dir.path().join("daemon.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// A fresh uuid per program image: it is minted on first use and lives in
    /// memory, so it survives anything except being replaced. Together with the
    /// pid it pins down exactly what happened — same pid and a new instance is
    /// an `exec` and nothing else.
    fn instance(&self) -> String {
        self.panes()
            .version()
            .expect("the daemon answers its version")
            .instance
    }

    fn await_ready(&self) {
        let deadline = Instant::now() + READY_WITHIN;
        loop {
            if self.panes().version().is_ok() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the daemon did not open its pane endpoint within {READY_WITHIN:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn size() -> WinSize {
    WinSize {
        cols: 100,
        rows: 30,
        cell_w: 8,
        cell_h: 16,
    }
}

fn interactive_shell() -> ShellSpec {
    ShellSpec {
        program: "/bin/sh".into(),
        args: Vec::new(),
        args_are_tty7_defaults: false,
    }
}

fn windows_contain(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn collect_until(session: &mut tty7_core::client::PaneSession, marker: &[u8]) -> Vec<u8> {
    let mut seen: Vec<u8> = Vec::new();
    loop {
        match session.recv() {
            Ok(DaemonMsg::Output(bytes)) | Ok(DaemonMsg::Snapshot(bytes)) => {
                seen.extend_from_slice(&bytes);
                if windows_contain(&seen, marker) {
                    return seen;
                }
            }
            Ok(DaemonMsg::Exited { code }) => panic!(
                "the pane exited ({code:?}) before {:?} appeared; saw {:?}",
                String::from_utf8_lossy(marker),
                String::from_utf8_lossy(&seen)
            ),
            Ok(_) => {}
            Err(e) => panic!(
                "the pane stream ended early: {e}; saw {:?}",
                String::from_utf8_lossy(&seen)
            ),
        }
    }
}

#[test]
fn a_handoff_keeps_the_process_the_pty_and_the_shell_that_is_on_it() {
    let daemon = Daemon::start();
    let panes = daemon.panes();
    let before_pid = daemon.recorded_pid().expect("the daemon records its pid");
    let before_instance = daemon.instance();

    let mut session = panes
        .spawn(None, size(), Some(interactive_shell()), None, None)
        .expect("spawn an interactive pane");
    let pane_id = session.pane_id();
    session
        .set_recv_timeout(Some(STREAM_WITHIN))
        .expect("bound the stream reads");

    // Put something in this shell's memory that exists nowhere else, and print
    // a marker so we know the shell has read its input before we hand over.
    session
        .input(b"tty7_kept=survivor; echo tty7_before_$tty7_kept\r")
        .expect("the shell takes input");
    collect_until(&mut session, b"tty7_before_survivor");
    drop(session);

    panes
        .hand_off(&Daemon::binary())
        .expect("the daemon hands over");
    daemon.await_ready();

    assert_eq!(
        daemon.recorded_pid(),
        Some(before_pid),
        "an exec keeps the process; a different pid here would mean the daemon stopped and \
         started, which is the thing this is supposed to avoid"
    );
    assert_ne!(
        daemon.instance(),
        before_instance,
        "and a *new image* has to be what is answering — without this the test would pass just \
         as well if the handoff had quietly done nothing at all"
    );

    let mut session = panes
        .attach(pane_id, size())
        .expect("the pane is still there under the same id");
    session
        .set_recv_timeout(Some(STREAM_WITHIN))
        .expect("bound the stream reads");

    // The replay is the ring the previous image was holding.
    let replayed = collect_until(&mut session, b"tty7_before_survivor");
    assert!(
        windows_contain(&replayed, b"tty7_before_survivor"),
        "the pane came back without the output it had before the handoff"
    );

    // And the variable. Only the shell that ran the first line can answer this;
    // the command line echoes back unexpanded, so the expansion in the output
    // is unambiguous.
    session
        .input(b"echo tty7_after_$tty7_kept\r")
        .expect("the shell still takes input");
    collect_until(&mut session, b"tty7_after_survivor");

    session.kill().expect("kill the pane");
}

#[test]
fn a_handoff_to_something_that_will_not_exec_leaves_the_daemon_serving() {
    let daemon = Daemon::start();
    let panes = daemon.panes();
    let before_pid = daemon.recorded_pid().expect("the daemon records its pid");
    let before_instance = daemon.instance();

    let mut session = panes
        .spawn(None, size(), Some(interactive_shell()), None, None)
        .expect("spawn an interactive pane");
    let pane_id = session.pane_id();
    session
        .set_recv_timeout(Some(STREAM_WITHIN))
        .expect("bound the stream reads");
    session
        .input(b"echo tty7_still_here\r")
        .expect("the shell takes input");
    collect_until(&mut session, b"tty7_still_here");

    let err = panes
        .hand_off(Path::new("/nonexistent/tty7-that-is-not-there"))
        .expect_err("a binary that cannot be executed must be reported, not assumed");
    assert!(
        err.to_string().contains("cannot become"),
        "a missing binary is refused before anything is given up; the refusal was {err}"
    );

    // The state is staged before the exec and the exec is the last step, so a
    // failure at that step has to cost nothing at all.
    assert_eq!(daemon.recorded_pid(), Some(before_pid));
    assert_eq!(
        daemon.instance(),
        before_instance,
        "nothing was replaced, so the same image has to still be answering"
    );
    session
        .input(b"echo tty7_unharmed\r")
        .expect("the pane still has its shell");
    collect_until(&mut session, b"tty7_unharmed");
    assert_eq!(
        session.pane_id(),
        pane_id,
        "the pane the caller was holding is the pane it still holds"
    );

    session.kill().expect("kill the pane");
}

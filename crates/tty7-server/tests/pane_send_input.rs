use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tty7_core::client::PaneClient;
use tty7_core::daemon::protocol::{DaemonMsg, ShellSpec, WinSize};

const READY_WITHIN: Duration = Duration::from_secs(30);
const STREAM_WITHIN: Duration = Duration::from_secs(30);
const EOF_WITHIN: Duration = Duration::from_secs(10);

struct Daemon {
    child: Child,
    dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Daemon {
        let dir = tempfile::TempDir::new().unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_tty7-server"))
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

    fn pane_endpoint(&self) -> PathBuf {
        let file = if cfg!(windows) {
            "daemon.port"
        } else {
            "daemon.sock"
        };
        self.dir.path().join(file)
    }

    fn panes(&self) -> PaneClient {
        PaneClient::at(self.pane_endpoint())
    }

    fn await_ready(&self) {
        let deadline = Instant::now() + READY_WITHIN;
        loop {
            if self.panes().version().is_ok() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "tty7-server did not open its pane endpoint within {READY_WITHIN:?}"
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

fn one_shot_shell(command: &str) -> ShellSpec {
    if cfg!(windows) {
        ShellSpec {
            program: "cmd.exe".into(),
            args: vec!["/d".into(), "/c".into(), command.into()],
            args_are_tty7_defaults: false,
        }
    } else {
        ShellSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), command.into()],
            args_are_tty7_defaults: false,
        }
    }
}

fn interactive_shell() -> ShellSpec {
    if cfg!(windows) {
        ShellSpec {
            program: "cmd.exe".into(),
            args: vec!["/d".into()],
            args_are_tty7_defaults: false,
        }
    } else {
        ShellSpec {
            program: "/bin/sh".into(),
            args: Vec::new(),
            args_are_tty7_defaults: false,
        }
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
                "pane exited ({code:?}) before {:?} appeared; saw {:?}",
                String::from_utf8_lossy(marker),
                String::from_utf8_lossy(&seen)
            ),
            Ok(_) => {}
            Err(e) => panic!(
                "pane stream ended early: {e}; saw {:?}",
                String::from_utf8_lossy(&seen)
            ),
        }
    }
}

#[test]
fn send_input_reaches_the_shell_without_displacing_the_controller() {
    let daemon = Daemon::start();
    let panes = daemon.panes();
    let mut session = panes
        .spawn(None, size(), Some(interactive_shell()), None, None)
        .expect("spawn an interactive pane");
    let pane_id = session.pane_id();
    session
        .set_recv_timeout(Some(STREAM_WITHIN))
        .expect("bound the stream reads");

    panes
        .send_input(pane_id, b"echo tty7_send_oneshot\r")
        .expect("one-shot input is acknowledged");
    collect_until(&mut session, b"tty7_send_oneshot");

    session
        .input(b"echo tty7_still_controller\r")
        .expect("the controller keeps its seat");
    collect_until(&mut session, b"tty7_still_controller");

    session.kill().expect("kill the pane");
}

#[test]
fn send_input_to_a_missing_pane_answers_an_error() {
    let daemon = Daemon::start();
    let err = daemon
        .panes()
        .send_input(u64::MAX, b"echo lost\r")
        .expect_err("input into a pane that never existed must fail");
    assert!(
        err.to_string().contains("no such pane"),
        "the refusal was {err}"
    );
}

#[test]
fn send_input_to_a_dead_pane_answers_an_error() {
    let daemon = Daemon::start();
    let panes = daemon.panes();
    let mut session = panes
        .spawn(None, size(), Some(one_shot_shell("exit 0")), None, None)
        .expect("spawn a one-shot pane");
    let pane_id = session.pane_id();
    session
        .set_recv_timeout(Some(STREAM_WITHIN))
        .expect("bound the stream reads");
    loop {
        match session.recv() {
            Ok(DaemonMsg::Exited { .. }) => break,
            Ok(_) => {}
            Err(e) => panic!("pane stream ended before Exited: {e}"),
        }
    }

    let err = panes
        .send_input(pane_id, b"echo too_late\r")
        .expect_err("input into an exited pane must fail");
    assert!(
        err.to_string().contains("not running"),
        "the refusal was {err}"
    );
}

#[test]
fn a_preempting_attach_closes_the_displaced_controller_and_drops_its_input() {
    let daemon = Daemon::start();
    let panes = daemon.panes();
    let mut first = panes
        .spawn(None, size(), Some(interactive_shell()), None, None)
        .expect("spawn an interactive pane");
    let pane_id = first.pane_id();
    first
        .set_recv_timeout(Some(STREAM_WITHIN))
        .expect("bound the stream reads");
    first
        .input(b"echo tty7_first_seated\r")
        .expect("the first controller types");
    collect_until(&mut first, b"tty7_first_seated");

    let mut second = panes.attach(pane_id, size()).expect("preempting attach");
    second
        .set_recv_timeout(Some(STREAM_WITHIN))
        .expect("bound the second stream reads");

    let _ = first.input(b"echo tty7_stale_input\r");

    // Best effort: the handover closes this connection immediately, and
    // setsockopt on a socket whose peer is gone fails (EINVAL on macOS). Not a
    // problem — the earlier STREAM_WITHIN bound is still in force, so the reads
    // below stay bounded either way, and an already-closed connection is
    // precisely what this test wants to see.
    let _ = first.set_recv_timeout(Some(EOF_WITHIN));
    let deadline = Instant::now() + EOF_WITHIN + Duration::from_secs(5);
    let eof = loop {
        match first.recv() {
            Ok(_) => {
                assert!(
                    Instant::now() < deadline,
                    "the displaced controller's stream never ended"
                );
            }
            Err(e) => break e,
        }
    };
    assert_ne!(
        eof.kind(),
        io::ErrorKind::TimedOut,
        "the displaced connection must be closed, not left dangling: {eof}"
    );
    assert_ne!(
        eof.kind(),
        io::ErrorKind::WouldBlock,
        "the displaced connection must be closed, not left dangling: {eof}"
    );

    second
        .input(b"echo tty7_second_alive\r")
        .expect("the new controller types");
    let seen = collect_until(&mut second, b"tty7_second_alive");
    assert!(
        !windows_contain(&seen, b"tty7_stale_input"),
        "input from the displaced controller must never reach the shell: {:?}",
        String::from_utf8_lossy(&seen)
    );

    second.kill().expect("kill the pane");
}

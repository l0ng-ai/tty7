//! An agent hook run inside a pane reports to the daemon over its socket, not
//! through the pane's tty — where the agent's own output can cut into it.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tty7_core::client::PaneClient;
use tty7_core::core::cli_agent::AgentStatus;
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
        let deadline = Instant::now() + READY_WITHIN;
        while daemon.panes().version().is_err() {
            assert!(Instant::now() < deadline, "the daemon never came up");
            std::thread::sleep(Duration::from_millis(50));
        }
        daemon
    }

    fn panes(&self) -> PaneClient {
        PaneClient::at(self.dir.path().join("daemon.sock"))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn hook_shell(pane_env: &str) -> ShellSpec {
    let hook = PathBuf::from(env!("CARGO_BIN_EXE_tty7-server"));
    let command = format!(
        "printf '%s' '{{\"session_id\":\"s-1\",\"permission_mode\":\"plan\"}}' \
         | {pane_env} '{}' agent-hook claude prompt-submit; echo hook_ran; exec cat",
        hook.display()
    );
    ShellSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), command],
        args_are_tty7_defaults: false,
    }
}

/// Runs the hook in a fresh pane and returns what the pane printed up to the
/// hook finishing, and the agent status the daemon reported meanwhile.
fn run_hook(pane_env: &str) -> (Vec<u8>, Option<AgentStatus>) {
    let daemon = Daemon::start();
    let size = WinSize {
        cols: 100,
        rows: 30,
        cell_w: 8,
        cell_h: 16,
    };
    let mut session = daemon
        .panes()
        .spawn(None, size, Some(hook_shell(pane_env)), None, None)
        .expect("spawn the pane");
    session
        .set_recv_timeout(Some(STREAM_WITHIN))
        .expect("bound the stream reads");
    let mut seen = Vec::new();
    let mut status = None;
    while !seen.windows(8).any(|w| w == b"hook_ran") {
        match session.recv().expect("the pane keeps streaming") {
            DaemonMsg::Output(bytes) | DaemonMsg::Snapshot(bytes) => seen.extend(bytes),
            DaemonMsg::AgentStatus(Some(s)) => status = Some(s.status),
            _ => {}
        }
    }
    // A report over the tty is parsed off the same output, just before it.
    while status.is_none() {
        match session.recv() {
            Ok(DaemonMsg::AgentStatus(Some(s))) => status = Some(s.status),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = session.kill();
    (seen, status)
}

fn carries_the_sequence(output: &[u8]) -> bool {
    output.windows(16).any(|w| w == b"tty7://cli-agent")
}

#[test]
fn a_hook_reports_over_the_socket_and_leaves_the_tty_alone() {
    let (output, status) = run_hook("");
    assert_eq!(status, Some(AgentStatus::Working));
    assert!(
        !carries_the_sequence(&output),
        "the report went through the tty: {:?}",
        String::from_utf8_lossy(&output)
    );
}

#[test]
fn a_hook_the_daemon_turns_away_falls_back_to_the_tty() {
    // A pane id this daemon never handed out: the report is refused, and the
    // hook writes the sequence to its tty as before.
    let (output, status) = run_hook("TTY7_PANE=999999");
    assert!(
        carries_the_sequence(&output),
        "{:?} {status:?}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(status, Some(AgentStatus::Working));
}

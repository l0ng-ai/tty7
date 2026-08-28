//! A pane's screen survives the daemon being stopped and started.
//!
//! The unit tests under `daemon::scrollback` cover the file: it round-trips, it
//! is trimmed from the front, the sweep keeps what it is told to keep. None of
//! that answers the question the feature exists for, which is whether a window
//! that comes back to a restarted daemon is shown what its panes had on them.
//! That answer involves a real daemon writing a real snapshot, dying, and a
//! second daemon handing the bytes to the client that asks — so this runs all
//! of it and reads the wire.
//!
//! The ordering is the whole difficulty. A client cannot ask for a restore
//! until the daemon is listening, so anything the daemon throws away *while
//! starting up* is thrown away before the only party who knows what is still
//! wanted has been able to say so.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tty7_core::client::{ControlClient, PaneClient};
use tty7_core::core::machine::PaneSeed;
use tty7_core::daemon::control::{ControlHello, ControlRequest, ReplyOk};
use tty7_core::daemon::protocol::{ClientMsg, DaemonMsg, RestoreFrom, ShellSpec, WinSize};
use tty7_core::daemon::transport;

const READY_WITHIN: Duration = Duration::from_secs(30);
const STREAM_WITHIN: Duration = Duration::from_secs(30);
const STOP_WITHIN: Duration = Duration::from_secs(30);

const MARKER: &[u8] = b"tty7_screen_kept";

/// One instance's directories, outliving the daemons that serve them — the
/// point of the test is what is on disk between two of them.
struct Instance {
    dir: tempfile::TempDir,
}

impl Instance {
    fn new() -> Instance {
        // No config: keeping each pane's screen is what the daemon does, not
        // something it is asked to do.
        Instance {
            dir: tempfile::TempDir::new().unwrap(),
        }
    }

    fn path(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// What the daemon writes down for clients to find it by: a socket on unix,
    /// a port-and-token file on Windows.
    fn endpoint(&self) -> PathBuf {
        #[cfg(unix)]
        let name = "daemon.sock";
        #[cfg(windows)]
        let name = "daemon.port";
        self.dir.path().join(name)
    }

    fn panes(&self) -> PaneClient {
        PaneClient::at(self.endpoint())
    }

    fn control_endpoint(&self) -> PathBuf {
        #[cfg(unix)]
        let name = "control.sock";
        #[cfg(windows)]
        let name = "control.port";
        self.dir.path().join(name)
    }

    fn snapshot_of(&self, pane_id: u64) -> Option<Vec<u8>> {
        std::fs::read(
            self.dir
                .path()
                .join("scrollback")
                .join(format!("{pane_id}.bin")),
        )
        .ok()
    }

    /// The tree a window would have left behind: one workspace, one tab, and
    /// that tab standing on the pane whose screen we want back.
    fn record_tab_on(&self, pane_id: u64) {
        let tree = format!(
            r#"{{
  "workspaces": [
    {{
      "id": "11111111-2222-3333-4444-555555555555",
      "name": null,
      "last_active": 1786343761,
      "tabs": [
        {{
          "id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
          "name": null,
          "sidebar_group": null,
          "root": {{ "Leaf": {{ "pane": {pane_id} }} }}
        }}
      ],
      "active_tab": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    }}
  ],
  "panes": [
    {{ "id": {pane_id}, "cwd": null, "title": "", "ssh_spec": null, "agent": null, "live": false }}
  ]
}}"#
        );
        std::fs::write(self.dir.path().join("machine.json"), tree).unwrap();
    }

    fn start(&self) -> Running {
        let child = Command::new(env!("CARGO_BIN_EXE_tty7-server"))
            .arg("--daemon")
            .arg("--config-dir")
            .arg(self.path())
            .env("TTY7_DATA_DIR", self.path())
            .env("TTY7_CONTROL_SOCK", self.path().join("control.sock"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start tty7-server --daemon");
        let running = Running {
            child,
            stopped: false,
        };
        let deadline = Instant::now() + READY_WITHIN;
        loop {
            if self.panes().version().is_ok() {
                return running;
            }
            assert!(
                Instant::now() < deadline,
                "the daemon did not open its pane endpoint within {READY_WITHIN:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Ask the daemon to go, the way the restart in Settings asks. This is the
    /// path that takes each pane's last snapshot on the way out, so a test that
    /// killed the process instead would be testing the periodic writer.
    fn stop(&self, mut running: Running) {
        let mut stream =
            transport::connect_endpoint_at(&self.endpoint()).expect("connect to ask for shutdown");
        ClientMsg::Shutdown
            .encode(&mut stream)
            .expect("send Shutdown");
        stream.flush().ok();
        drop(stream);

        let deadline = Instant::now() + STOP_WITHIN;
        loop {
            match running.child.try_wait() {
                Ok(Some(_)) => {
                    running.stopped = true;
                    return;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(None) => panic!("the daemon did not exit within {STOP_WITHIN:?}"),
                Err(e) => panic!("waiting for the daemon failed: {e}"),
            }
        }
    }

    /// The `Spawn` a window sends for a pane whose `Attach` found nothing: a
    /// new shell, asked to open showing what the dead one had.
    fn spawn_restoring(&self, dead: u64) -> (u64, Vec<u8>) {
        let mut stream =
            transport::connect_endpoint_at(&self.endpoint()).expect("connect to spawn");
        ClientMsg::Spawn {
            cwd: None,
            size: size(),
            shell: Some(interactive_shell()),
            owner: None,
            workspace: None,
            restore: Some(RestoreFrom {
                pane_id: dead,
                banner: Some("the shell below is new".to_string()),
            }),
            allow_remote_clipboard_write: false,
        }
        .encode(&mut stream)
        .expect("send Spawn");

        let pane_id = match DaemonMsg::read(&mut stream) {
            Ok(DaemonMsg::Spawned { pane_id }) => pane_id,
            other => panic!("expected Spawned, got {other:?}"),
        };

        // Everything the daemon replays sits in the socket ahead of whatever
        // the new shell writes, so a bounded drain is enough: the replay is
        // already queued by the time `Spawned` is read.
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("bound the drain");
        let mut replayed = Vec::new();
        while let Ok(msg) = DaemonMsg::read(&mut stream) {
            match msg {
                DaemonMsg::Snapshot(bytes) | DaemonMsg::Output(bytes) => {
                    replayed.extend_from_slice(&bytes)
                }
                _ => {}
            }
        }
        (pane_id, replayed)
    }
}

struct Running {
    child: Child,
    stopped: bool,
}

impl Drop for Running {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
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
    #[cfg(unix)]
    let program = "/bin/sh";
    #[cfg(windows)]
    let program = "cmd.exe";
    ShellSpec {
        program: program.into(),
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

/// Put a marker on a pane's screen and give the daemon back the pane's id.
fn pane_showing_the_marker(instance: &Instance) -> u64 {
    let mut session = instance
        .panes()
        .spawn(None, size(), Some(interactive_shell()), None, None)
        .expect("spawn a pane");
    session
        .set_recv_timeout(Some(STREAM_WITHIN))
        .expect("bound the stream reads");
    let pane_id = session.pane_id();
    session
        .input(format!("echo {}\r", String::from_utf8_lossy(MARKER)).as_bytes())
        .expect("the shell takes input");
    collect_until(&mut session, MARKER);
    // Detach rather than kill: the pane outlives this connection, which is the
    // state a daemon restart finds its panes in.
    session.detach().expect("detach");
    pane_id
}

/// The case the feature is for: nothing has told the new daemon anything yet,
/// because the window that would tell it is still waiting for it to listen.
#[test]
fn a_restarted_daemon_still_has_the_screen_when_the_window_asks() {
    let instance = Instance::new();
    let running = instance.start();
    let dead = pane_showing_the_marker(&instance);
    instance.stop(running);

    let stored = instance
        .snapshot_of(dead)
        .expect("the shutdown wrote the pane's screen");
    assert!(
        windows_contain(&stored, MARKER),
        "the snapshot on disk does not hold the pane's screen"
    );

    let _restarted = instance.start();
    let (_new_pane, replayed) = instance.spawn_restoring(dead);
    assert!(
        windows_contain(&replayed, MARKER),
        "the restarted daemon did not give the pane back its screen; \
         the client received {:?}",
        String::from_utf8_lossy(&replayed)
    );
}

/// What a pane is running has to reach the tree, because that is the only
/// place a window rebuilding the pane can read it from. Without it the rebuild
/// falls back to the default shell, which is how a restart turns a bash pane
/// into a PowerShell one.
#[test]
fn the_tree_records_the_shell_a_pane_is_running() {
    let instance = Instance::new();
    let _running = instance.start();
    let pane = pane_showing_the_marker(&instance);

    // A pane reaches the tree by being put in a tab, which is what the window
    // does right after it spawns one — carrying the same seed it spawned with.
    // Until then the daemon has nothing to record its facts against, so this is
    // the pane's first chance to say what it is running, and for a pane that
    // then sits at a prompt it is the only one: the daemon's own observation
    // rides on a fact *changing*, and a pane's shell never does.
    let control = ControlClient::connect_at(
        &instance.control_endpoint(),
        &ControlHello::host_rpc("probe", "probe"),
    )
    .expect("control handshake");
    let ws = match control
        .request(ControlRequest::WorkspaceCreate {
            name: Some("probe".into()),
            workspace: None,
        })
        .expect("create a workspace")
    {
        ReplyOk::WorkspaceTree(ws) => *ws,
        other => panic!("expected WorkspaceTree, got {other:?}"),
    };
    control
        .request(ControlRequest::TabCreate {
            workspace: ws.id,
            at: None,
            pane: PaneSeed {
                shell: Some(interactive_shell()),
                ..PaneSeed::bare(pane)
            },
            tab: None,
        })
        .expect("put the pane in a tab");

    let wanted = interactive_shell().program;
    let deadline = Instant::now() + STREAM_WITHIN;
    loop {
        let tree =
            std::fs::read_to_string(instance.path().join("machine.json")).unwrap_or_default();
        // The record is keyed by pane id and the program is a plain string in
        // it; matching on both together is enough to say this pane's shell —
        // not some other pane's — reached the tree.
        if tree.contains(&format!("\"id\": {pane}")) && tree.contains(&wanted) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the tree never recorded pane {pane}'s shell ({wanted}); it holds: {tree}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The same thing for an instance whose tree still names the pane. This one
/// passes on its own merits today; it is here so a fix for the case above
/// cannot be one that quietly stops honouring the tree.
#[test]
fn a_screen_comes_back_when_the_tree_still_names_its_pane() {
    let instance = Instance::new();
    let running = instance.start();
    let dead = pane_showing_the_marker(&instance);
    instance.stop(running);
    instance.record_tab_on(dead);

    let _restarted = instance.start();
    let (_new_pane, replayed) = instance.spawn_restoring(dead);
    assert!(
        windows_contain(&replayed, MARKER),
        "a pane the tree still names came back blank; the client received {:?}",
        String::from_utf8_lossy(&replayed)
    );
}

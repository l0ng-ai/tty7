//! Opt-in regression through the Windows daemon's real native SSH router and
//! the GUI's actual terminal reader. No installed client/server is changed.
use super::*;
use crate::core::machine::PaneSeed;
use crate::daemon::control::{ControlHello, ControlRequest, ReplyOk, WorkspaceProof};
use crate::daemon::router::RouteHeader;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tty7_core::host::remote::RemoteHost;

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Closing the fixture's stdin lets its finally stop its private daemon.
        drop(self.0.stdin.take());
        let until = Instant::now() + Duration::from_secs(8);
        while self.0.try_wait().ok().flatten().is_none() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child(command: &mut Command) -> ChildGuard {
    crate::core::proc::hide_console(command);
    ChildGuard(command.spawn().unwrap())
}

fn private_local_daemon(directory: &std::path::Path) -> ChildGuard {
    let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/tty7-app.exe");
    child(
        Command::new(app)
            .args(["--daemon", "--config-dir"])
            .arg(directory.join("config"))
            .env("TTY7_DATA_DIR", directory.join("data"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(15);
    while !ready() {
        assert!(Instant::now() < until, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn text_of(pane: &RemoteTerminal) -> String {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    let term = pane.term.lock();
    let mut text = String::new();
    for row in 0..term.screen_lines() {
        for col in 0..term.columns() {
            text.push(term.grid()[Line(row as i32)][Column(col)].c);
        }
        text.push('\n');
    }
    text
}

fn control(header: &RouteHeader) -> Arc<RemoteHost> {
    let mut stream = transport::connect().unwrap();
    crate::daemon::router::negotiate(&mut stream, header).unwrap();
    RemoteHost::over_tcp(
        stream,
        "isolated-windows-reconnect",
        &ControlHello::host_rpc(uuid::Uuid::new_v4().to_string(), "isolated-windows-test"),
    )
    .unwrap()
}

fn resume(
    host: &RemoteHost,
    workspace: crate::core::session::WorkspaceId,
    proof: Option<WorkspaceProof>,
    header: &RouteHeader,
) -> (WorkspaceProof, PaneRoute) {
    let ReplyOk::WorkspaceLease {
        proof, pane_token, ..
    } = host
        .client()
        .call(ControlRequest::WorkspaceResume {
            id: workspace.to_string(),
            proof,
        })
        .unwrap()
    else {
        panic!("workspace was not resumed")
    };
    (
        proof,
        PaneRoute::Remote {
            header: Box::new(header.clone().for_pane()),
            authorization: Some(crate::daemon::protocol::PaneAuthorization {
                workspace,
                token: pane_token,
            }),
            resize_echo: true,
        },
    )
}

#[test]
#[ignore = "requires explicitly configured SSH account and isolated remote candidate; run alone"]
fn windows_native_ssh_restores_the_original_workspace_and_process() {
    let account =
        std::env::var("TTY7_REMOTE_TEST_ACCOUNT").expect("explicit test account required");
    let root = std::env::var("TTY7_REMOTE_TEST_ROOT").expect("private remote artifacts required");
    assert!(root.starts_with("/tmp/tty7-774-") && !root.contains('\''));
    let spec: NativeSshSpec = serde_json::from_str(
        &std::env::var("TTY7_REMOTE_TEST_SPEC").expect("explicit native SSH spec required"),
    )
    .unwrap();
    assert!(
        spec.verify_host_keys,
        "never bypass SSH host verification for the test"
    );
    let local = tempfile::tempdir().unwrap();
    crate::core::config::set_config_dir(local.path().join("config"));
    let mut local_daemon = private_local_daemon(local.path());
    wait_for("isolated Windows daemon", || transport::connect().is_ok());
    let server = format!("{root}/server-startup-c10p9");
    let mut fixture = child(Command::new("ssh").args([
        "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "StrictHostKeyChecking=yes",
        &account, &format!("python3 '{root}/remote_774_fixture.py' --server '{server}' --artifacts '{root}'"),
    ]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()));
    let mut line = String::new();
    BufReader::new(fixture.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let info: serde_json::Value =
        serde_json::from_str(&line).expect("private remote fixture started");
    let directory = info["directory"].as_str().unwrap();
    assert!(directory.starts_with(&format!("{root}/windows-")) && !directory.contains('\''));
    let mut header = RouteHeader::ssh(spec);
    header.server_command = Some(format!(
        "'{server}' --config-dir '{directory}' --stdio --bridge"
    ));
    let host = control(&header);
    let instance = host.peer().instance.clone();
    let ReplyOk::WorkspaceTree(workspace) = host
        .client()
        .call(ControlRequest::WorkspaceCreate {
            name: Some("windows-reconnect-original".into()),
            workspace: None,
        })
        .unwrap()
    else {
        panic!("workspace not created")
    };
    let workspace = workspace.id;
    let (proof, route) = resume(&host, workspace, None, &header);
    let size = TermSize::new(100, 30);
    let (mut pane, pane_id) = RemoteTerminal::spawn_on(&route, size, 8, 16, Some(directory.into()), Some(ShellSpec {
        program: "/bin/sh".into(), args: vec!["-c".into(),
            "stty -echo; printf 'ORIGINAL-PID=%s\\n' $$; while IFS= read -r line; do printf 'REPLY:%s:PID=%s\\n' \"$line\" $$; done".into()],
        args_are_tty7_defaults: false,
    }), Some(workspace.to_string()), None).unwrap();
    wait_for("original process output", || {
        text_of(&pane).contains("ORIGINAL-PID=")
    });
    let original = text_of(&pane);
    let pid = original
        .split("ORIGINAL-PID=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    let seed = PaneSeed::bare(pane_id);
    host.client()
        .call(ControlRequest::TabCreate {
            workspace,
            at: None,
            pane: seed,
            tab: None,
        })
        .unwrap();
    eprintln!(
        "connected: private daemon {}, pane {pane_id}, original process {pid}",
        info["pid"]
    );
    for cycle in 0..3 {
        if cycle == 1 {
            // Tear down the actual SSH transport, not just a control channel.
            // This is our private Windows daemon, never the installed daemon.
            local_daemon.0.kill().unwrap();
            local_daemon.0.wait().unwrap();
            local_daemon = private_local_daemon(local.path());
            wait_for("replacement private Windows daemon", || {
                transport::connect().is_ok()
            });
            eprintln!("cycle 1: private Windows daemon replaced; native SSH transport was lost");
        }
        // Drop the control route as a network disconnect would, not WorkspaceDetach.
        host.client().close();
        pane.detach_link();
        let next = control(&header);
        assert_eq!(
            next.peer().instance,
            instance,
            "reconnect must not replace the remote daemon"
        );
        let (_, next_route) = resume(&next, workspace, Some(proof.clone()), &header);
        let Err(denied) = RemoteTerminal::open_relink(&route, pane_id, size, 8, 16) else {
            panic!("a retired workspace credential cannot attach")
        };
        assert!(
            !attach_refused(&denied),
            "an expired lease does not mean the process is gone"
        );
        let ReplyOk::MachineTree(machine) = next.client().call(ControlRequest::MachineGet).unwrap()
        else {
            panic!()
        };
        let ws = machine
            .workspaces
            .iter()
            .find(|ws| ws.id == workspace)
            .unwrap();
        assert_eq!(ws.tabs.len(), 1);
        assert!(machine.panes.iter().any(|p| p.id == pane_id && p.live));
        let (stream, buffered) =
            RemoteTerminal::open_relink(&next_route, pane_id, size, 8, 16).unwrap();
        pane.adopt_relink(stream, buffered, &next_route, size, 8, 16)
            .unwrap();
        pane.write(format!("cycle-{cycle}\n").into_bytes());
        let expected = format!("REPLY:cycle-{cycle}:PID={pid}");
        wait_for("same process accepts input after reconnect", || {
            text_of(&pane).contains(&expected)
        });
        eprintln!("cycle {cycle}: original workspace, pane and process retained; input works");
        next.client().close();
    }
    drop(pane);
    let host = control(&header);
    let (_, route) = resume(&host, workspace, Some(proof.clone()), &header);
    let vim_path = format!("{directory}/windows-vim.txt");
    let (mut vim, vim_id) = RemoteTerminal::spawn_on(
        &route,
        size,
        8,
        16,
        Some(directory.into()),
        Some(ShellSpec {
            program: "/usr/bin/vim".into(),
            args: vec![
                "-Nu".into(),
                "NONE".into(),
                "-i".into(),
                "NONE".into(),
                "-n".into(),
                vim_path.clone(),
            ],
            args_are_tty7_defaults: false,
        }),
        Some(workspace.to_string()),
        None,
    )
    .unwrap();
    wait_for("Vim entered alternate screen", || {
        vim.term.lock().mode().contains(TermMode::ALT_SCREEN)
    });
    vim.write(b"iTTY7-WINDOWS-UNSAVED\x1b".to_vec());
    wait_for("Vim holds unsaved text", || {
        text_of(&vim).contains("TTY7-WINDOWS-UNSAVED")
    });
    vim.resize(TermSize::new(120, 36), 8, 16);
    vim.resize(TermSize::new(80, 24), 8, 16);
    host.client().close();
    drop(vim);
    let next = control(&header);
    let (_, route) = resume(&next, workspace, Some(proof.clone()), &header);
    let vim = RemoteTerminal::attach_on(&route, TermSize::new(80, 24), 8, 16, vim_id).unwrap();
    wait_for("reattached Vim uses the current 80x24 viewport", || {
        use alacritty_terminal::grid::Dimensions;
        let term = vim.term.lock();
        term.columns() == 80 && term.screen_lines() == 24
    });
    wait_for(
        "Vim's unsaved buffer survived a new client terminal",
        || text_of(&vim).contains("TTY7-WINDOWS-UNSAVED"),
    );
    vim.write(b":".to_vec());
    wait_for("Vim command colon is on the last row", || {
        text_of(&vim)
            .lines()
            .nth(23)
            .is_some_and(|row| row.starts_with(':'))
    });
    vim.write(b"wq".to_vec());
    wait_for("Vim wq stays beside the colon", || {
        text_of(&vim)
            .lines()
            .nth(23)
            .is_some_and(|row| row.starts_with(":wq"))
    });
    assert_eq!(vim.term.lock().grid().cursor.point.line.0, 23);
    vim.write(b"\r".to_vec());
    wait_for("Vim saved and exited", || vim.child_exited());
    let saved = next
        .client()
        .call_full(
            ControlRequest::ReadFile {
                path: vim_path,
                max_bytes: 1024,
            },
            &[],
        )
        .unwrap();
    assert_eq!(saved.blob, b"TTY7-WINDOWS-UNSAVED\n");
    eprintln!(
        "Vim: unsaved buffer survived resize/reconnect; :wq is on row 24; saved bytes verified"
    );
    drop(vim);

    let (btop, btop_id) = RemoteTerminal::spawn_on(
        &route,
        size,
        8,
        16,
        Some(directory.into()),
        Some(ShellSpec {
            program: "/usr/bin/btop".into(),
            args: vec!["--utf-force".into(), "-u".into(), "100".into()],
            args_are_tty7_defaults: false,
        }),
        Some(workspace.to_string()),
        None,
    )
    .unwrap();
    wait_for(
        "btop entered alternate screen and enabled mouse reporting",
        || {
            let t = btop.term.lock();
            t.mode().contains(TermMode::ALT_SCREEN) && t.mode().intersects(TermMode::MOUSE_MODE)
        },
    );
    let relevant = TermMode::ALT_SCREEN
        | TermMode::MOUSE_MODE
        | TermMode::SGR_MOUSE
        | TermMode::UTF8_MOUSE
        | TermMode::ALTERNATE_SCROLL;
    let before = *btop.term.lock().mode() & relevant;
    // Observe the real btop byte stream without stealing its controller.
    // The byte budget, not elapsed time, proves we crossed the old replay cap.
    let mut observer = connect_routed(&route).unwrap();
    ClientMsg::Observe {
        pane_id: btop_id,
        size: win_size(size, 8, 16),
    }
    .encode(&mut observer)
    .unwrap();
    observer
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut output_bytes = 0;
    let mut reported = 0;
    while output_bytes < 9 * 1024 * 1024 {
        assert!(
            Instant::now() < deadline,
            "btop did not emit 9 MiB within the test budget: {output_bytes}"
        );
        match DaemonMsg::read(&mut observer) {
            Ok(DaemonMsg::Snapshot(bytes) | DaemonMsg::Output(bytes)) => {
                output_bytes += bytes.len()
            }
            Ok(DaemonMsg::Error(error)) => panic!("observing btop: {error}"),
            Ok(DaemonMsg::Exited { .. }) => panic!("btop exited before the long-run check"),
            Ok(_) => {}
            Err(error) if would_block(&error) => {}
            Err(error) => panic!("reading real btop output: {error}"),
        }
        if output_bytes / (1024 * 1024) > reported {
            reported = output_bytes / (1024 * 1024);
            eprintln!("real btop output: {reported} MiB");
        }
    }
    drop(observer);
    next.client().close();
    drop(btop);
    let last = control(&header);
    let (_, route) = resume(&last, workspace, Some(proof), &header);
    let btop = RemoteTerminal::attach_on(&route, size, 8, 16, btop_id).unwrap();
    wait_for(
        "btop long-run reconnect preserved screen/mouse routing modes",
        || *btop.term.lock().mode() & relevant == before,
    );
    eprintln!("btop: after {output_bytes} bytes, a new client restored ALT_SCREEN and mouse modes");
    {
        let mut term = btop.term.lock();
        term.scroll_display(alacritty_terminal::grid::Scroll::Delta(5));
        assert_eq!(
            term.grid().display_offset(),
            0,
            "a restored btop cannot scroll into primary history"
        );
    }
    btop.write(b"q".to_vec());
    wait_for("test btop exited", || btop.child_exited());
    assert!(!btop.term.lock().mode().contains(TermMode::ALT_SCREEN));
    drop(btop);
    last.client().close();
    // The remote fixture cleans its own process group even if an assertion fails.
    drop(fixture);
    // Explicit local shutdown only addresses the private config above.
    if let Ok(mut stream) = transport::connect() {
        ClientMsg::Access(crate::daemon::protocol::PaneAccess::Manage)
            .encode(&mut stream)
            .unwrap();
        ClientMsg::Shutdown.encode(&mut stream).unwrap();
    }
    wait_for("private Windows daemon shut down", || {
        local_daemon.0.try_wait().unwrap().is_some()
    });
}

//! Guards for #653: a daemon that lingers after unlinking its endpoint must
//! stay findable and reapable, or its singleton lock makes every later launch
//! stand down and time out red with no way back short of `pkill`.
//!
//! Unix-only: both tests drive the daemon through unix sockets and signals.

#![cfg(unix)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tty7_core::client::PaneClient;
use tty7_core::daemon::protocol::ClientMsg;

const READY_WITHIN: Duration = Duration::from_secs(30);
const EXIT_WITHIN: Duration = Duration::from_secs(10);

fn spawn_daemon(dir: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_tty7-server"))
        .arg("--daemon")
        .arg("--config-dir")
        .arg(dir)
        .env("TTY7_DATA_DIR", dir)
        .env("TTY7_CONTROL_SOCK", dir.join("control.sock"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start tty7-server --daemon")
}

fn await_ready(dir: &Path) {
    let endpoint = dir.join("daemon.sock");
    let deadline = Instant::now() + READY_WITHIN;
    while PaneClient::at(&endpoint).version().is_err() || !dir.join("daemon.pid").exists() {
        assert!(
            Instant::now() < deadline,
            "tty7-server did not open its endpoint within {READY_WITHIN:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn send_shutdown(endpoint: &Path) {
    use std::io::Write as _;
    let mut stream =
        std::os::unix::net::UnixStream::connect(endpoint).expect("connect to the daemon");
    ClientMsg::Access(tty7_core::daemon::protocol::PaneAccess::Manage)
        .encode(&mut stream)
        .unwrap();
    ClientMsg::Shutdown
        .encode(&mut stream)
        .expect("send Shutdown");
    stream.flush().expect("flush Shutdown");
}

fn await_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + EXIT_WITHIN;
    loop {
        if let Some(status) = child.try_wait().expect("query the daemon's state") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the daemon did not exit within {EXIT_WITHIN:?} of Shutdown");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The pidfile must outlive the daemon's own cleanup: once `daemon.sock` is
/// unlinked it is the only name anything has for the process, and an exit
/// that stalls after the cleanup (#653) is findable through it — deleting it
/// there is what turned a lingering process into a permanent lockout.
#[test]
fn a_clean_shutdown_keeps_the_pidfile_until_the_process_is_gone() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut child = spawn_daemon(dir.path());
    await_ready(dir.path());
    let pid = child.id().to_string();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("daemon.pid"))
            .unwrap()
            .trim(),
        pid,
        "the pidfile names the running daemon"
    );

    send_shutdown(&dir.path().join("daemon.sock"));
    let status = await_exit(&mut child);

    assert!(status.success(), "clean shutdown exits cleanly: {status:?}");
    assert!(
        !dir.path().join("daemon.sock").exists(),
        "shutdown unlinks the endpoint"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("daemon.pid"))
            .unwrap()
            .trim(),
        pid,
        "the pidfile survives the daemon's cleanup; the reap in spawn::stop \
         and ensure_running deletes it once the process is confirmed gone"
    );
}

/// `spawn::stop` must reap a daemon that stopped listening but never exited
/// (#653) — and without the shutdown wait: only a shutdown that was actually
/// delivered earns `PROCESS_EXIT_TIMEOUT`, the time an *asked* daemon gets
/// to finish exiting. To a survivor stop() could not even connect to, that
/// wait was five seconds of watching nothing move (#667); the reap's own
/// SIGTERM window is all the grace such a process gets.
///
/// (The companion ordering — the pidfile already gone when the reap needs a
/// pid — is pinned by the `daemon_reap` suite through the seat record.)
#[test]
fn stop_reaps_an_unreachable_daemon_without_the_shutdown_wait() {
    let dir = tempfile::TempDir::new().unwrap();
    tty7_core::core::config::set_config_dir(dir.path().to_path_buf());
    let child = spawn_daemon(dir.path());
    let pid = child.id();
    await_ready(dir.path());

    // Collect the child the moment it dies: outside tests the daemon is
    // nobody's child, and a zombie would read as alive to the reap's
    // liveness poll.
    let waiter = std::thread::spawn(move || {
        let mut child = child;
        child.wait()
    });

    // The #653 state, as stop() meets it: the endpoint is gone before stop()
    // can ask for a shutdown, and the process lives on.
    std::fs::remove_file(dir.path().join("daemon.sock")).unwrap();

    let stop_started = Instant::now();
    tty7_core::daemon::spawn::stop();

    // Well under spawn.rs's PROCESS_EXIT_TIMEOUT (5s): a stop() that pays
    // that wait for a shutdown it never delivered has regressed.
    assert!(
        stop_started.elapsed() < Duration::from_secs(4),
        "stop() spent {:?} on a daemon it never reached — the exit wait must \
         be earned by a delivered shutdown",
        stop_started.elapsed()
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        if Instant::now() >= deadline {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            panic!("stop() left the daemon (pid {pid}) holding the singleton lock");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    waiter
        .join()
        .unwrap()
        .expect("collect the reaped daemon's exit");
}

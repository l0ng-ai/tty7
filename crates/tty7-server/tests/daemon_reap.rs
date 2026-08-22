//! Guards for #667: a daemon can survive with its endpoint unlinked and its
//! pidfile gone, holding the singleton seat against every later launch. The
//! reap must still find it — through the pid recorded in the lock file — and
//! must still recognise it when its executable has been deleted under it,
//! which is what every update that replaces the installation does.
//!
//! Unix-only: both tests drive the daemon through unix sockets and signals.
//! One process-wide config dir (`set_config_dir` is first-wins), so the tests
//! serialize on a mutex and each cleans up the daemon it started.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tty7_core::client::PaneClient;

const READY_WITHIN: Duration = Duration::from_secs(30);
const DEAD_WITHIN: Duration = Duration::from_secs(5);

/// The one config dir every test here shares, pinned into `tty7_core` on
/// first use. `set_config_dir` is first-wins for the whole process, so a
/// per-test directory would silently leave later tests reaping in the first
/// test's directory.
fn pinned_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        sweep_dead_runs();
        let dir = std::env::temp_dir().join(format!("tty7-reap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the shared config dir");
        tty7_core::core::config::set_config_dir(dir.clone());
        dir
    })
}

/// Removes `tty7-reap-<pid>` directories whose creating test run is gone —
/// the pid in the name keeps concurrent runs apart, and is also what says a
/// leftover is safe to delete.
fn sweep_dead_runs() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("tty7-reap-"))
            .and_then(|pid| pid.parse::<libc::pid_t>().ok())
            .filter(|&pid| pid > 0)
        else {
            continue;
        };
        let gone = unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn serialized() -> std::sync::MutexGuard<'static, ()> {
    static GATE: Mutex<()> = Mutex::new(());
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn clear_stale_files(dir: &Path) {
    for name in ["daemon.sock", "daemon.pid", "control.sock"] {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

fn spawn_daemon_from(exe: &Path, dir: &Path) -> Child {
    Command::new(exe)
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

/// Collects the child the moment it dies: outside tests the daemon is
/// nobody's child, and a zombie would read as alive to the reap's liveness
/// poll — and to this test's.
fn collect_on_exit(
    child: Child,
) -> std::thread::JoinHandle<std::io::Result<std::process::ExitStatus>> {
    std::thread::spawn(move || {
        let mut child = child;
        child.wait()
    })
}

fn assert_dies(pid: u32, what: &str) {
    let deadline = Instant::now() + DEAD_WITHIN;
    while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        if Instant::now() >= deadline {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            panic!("{what}: the daemon (pid {pid}) is still holding the seat");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The #667 state itself: endpoint unlinked, pidfile gone, the seat still
/// held by a live daemon. `stop` must find the survivor through the pid the
/// lock file records and reap it — and a fresh daemon must then be able to
/// take the seat.
#[test]
fn a_seat_holder_with_no_pidfile_is_still_found_and_reaped() {
    let _gate = serialized();
    let dir = pinned_dir();
    clear_stale_files(dir);

    let child = spawn_daemon_from(Path::new(env!("CARGO_BIN_EXE_tty7-server")), dir);
    let pid = child.id();
    await_ready(dir);
    assert_eq!(
        std::fs::read_to_string(dir.join("daemon.lock"))
            .unwrap()
            .trim(),
        pid.to_string(),
        "the lock file names the daemon holding the seat"
    );
    let waiter = collect_on_exit(child);

    // What #667 reports after quit-and-stop: both names for the process are
    // gone; only the seat (and the pid recorded in it) remains.
    std::fs::remove_file(dir.join("daemon.sock")).unwrap();
    std::fs::remove_file(dir.join("daemon.pid")).unwrap();

    tty7_core::daemon::spawn::stop();

    assert_dies(pid, "stop() with no pidfile");
    waiter
        .join()
        .unwrap()
        .expect("collect the reaped daemon's exit");

    // The user-visible acceptance: the next launch gets the seat instead of
    // standing down and timing out red.
    let second = spawn_daemon_from(Path::new(env!("CARGO_BIN_EXE_tty7-server")), dir);
    let second_pid = second.id();
    await_ready(dir);
    let waiter = collect_on_exit(second);
    tty7_core::daemon::spawn::stop();
    assert_dies(second_pid, "cleanup stop");
    waiter.join().unwrap().expect("collect the second daemon");
}

/// `reap_stranded` is the road the GUI's `ensure_running` and `tty7 server
/// start` take into the #667 state; it must clear the survivor the same way
/// `stop` does — after granting the grace a daemon mid-handoff would need.
#[test]
fn reap_stranded_clears_a_seat_holder_with_no_pidfile() {
    let _gate = serialized();
    let dir = pinned_dir();
    clear_stale_files(dir);

    let child = spawn_daemon_from(Path::new(env!("CARGO_BIN_EXE_tty7-server")), dir);
    let pid = child.id();
    await_ready(dir);
    let waiter = collect_on_exit(child);

    std::fs::remove_file(dir.join("daemon.sock")).unwrap();
    std::fs::remove_file(dir.join("daemon.pid")).unwrap();

    tty7_core::daemon::spawn::reap_stranded();

    assert_dies(pid, "reap_stranded() with no pidfile");
    waiter
        .join()
        .unwrap()
        .expect("collect the reaped daemon's exit");
    assert_eq!(
        std::fs::read_to_string(dir.join("daemon.lock")).unwrap(),
        "",
        "a confirmed reap clears the dead holder's record from the lock file"
    );
}

/// The grace's other half: a holder that answers the handshake is healthy —
/// mid-startup, mid-handoff, or simply fine — and `reap_stranded` must leave
/// it completely alone. This is the guard against the reap ending a daemon
/// that is carrying every live session across an exec.
#[test]
fn a_holder_that_answers_the_handshake_is_left_alone() {
    let _gate = serialized();
    let dir = pinned_dir();
    clear_stale_files(dir);

    let child = spawn_daemon_from(Path::new(env!("CARGO_BIN_EXE_tty7-server")), dir);
    let pid = child.id();
    await_ready(dir);
    let waiter = collect_on_exit(child);

    tty7_core::daemon::spawn::reap_stranded();

    assert!(
        unsafe { libc::kill(pid as libc::pid_t, 0) } == 0,
        "a healthy daemon must survive reap_stranded untouched"
    );
    assert!(
        PaneClient::at(dir.join("daemon.sock")).version().is_ok(),
        "and still be serving on its endpoint"
    );

    tty7_core::daemon::spawn::stop();
    assert_dies(pid, "cleanup stop");
    waiter.join().unwrap().expect("collect the daemon");
}

/// The startup grace must not excuse a holder that merely *connects*: a
/// wedged daemon's listener still completes connections out of the kernel's
/// backlog while answering nothing. Health is an answered handshake — a
/// live, connectable, silent holder is exactly what the reap is for.
#[test]
fn a_connectable_holder_that_answers_nothing_is_still_reaped() {
    let _gate = serialized();
    let dir = pinned_dir();
    clear_stale_files(dir);

    let child = spawn_daemon_from(Path::new(env!("CARGO_BIN_EXE_tty7-server")), dir);
    let pid = child.id();
    await_ready(dir);
    let waiter = collect_on_exit(child);

    // Frozen mid-service: the kernel keeps completing connections on its
    // listener, the process answers nothing — the closest reproducible stand-
    // in for a daemon wedged in its main loop.
    assert_eq!(
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGSTOP) },
        0,
        "freeze the daemon"
    );
    assert!(
        std::os::unix::net::UnixStream::connect(dir.join("daemon.sock")).is_ok(),
        "a frozen daemon's endpoint still connects — that is the trap"
    );

    tty7_core::daemon::spawn::reap_stranded();

    assert_dies(pid, "reap_stranded() against a connectable, silent holder");
    waiter
        .join()
        .unwrap()
        .expect("collect the reaped daemon's exit");
}

/// A daemon whose executable was deleted under it — every update that
/// replaces the installation — must still read as ours. `proc_pidpath` fails
/// outright for such a process on macOS, and treating that as "not our
/// daemon" dropped the pidfile without reaping anyone: the other road into
/// the #667 lockout.
#[test]
fn a_daemon_running_from_a_deleted_executable_is_still_ours_to_reap() {
    let _gate = serialized();
    let dir = pinned_dir();
    clear_stale_files(dir);

    let bin_dir = dir.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let copied = bin_dir.join("tty7-server");
    std::fs::copy(env!("CARGO_BIN_EXE_tty7-server"), &copied).unwrap();

    let child = spawn_daemon_from(&copied, dir);
    let pid = child.id();
    await_ready(dir);
    let waiter = collect_on_exit(child);

    std::fs::remove_file(&copied).unwrap();
    // Unlinked so stop() cannot simply ask for a shutdown: the reap's
    // identity check is the code under test, and it only runs against a
    // process that would not die politely. The pidfile stays — the pid
    // source here is the ordinary one; what is broken is the executable.
    std::fs::remove_file(dir.join("daemon.sock")).unwrap();

    tty7_core::daemon::spawn::stop();

    assert_dies(pid, "stop() with the executable deleted");
    waiter
        .join()
        .unwrap()
        .expect("collect the reaped daemon's exit");
    assert!(
        !dir.join("daemon.pid").exists(),
        "a confirmed reap clears the pidfile"
    );
}

//! GUI-side daemon launcher: make sure the persistent terminal daemon is running
//! before the GUI tries to connect, auto-spawning it as a *detached* background
//! process if it isn't.
//!
//! The daemon (`tty7 --daemon`, see `main.rs`) is a long-lived process that owns
//! all PTYs and outlives the GUI. The GUI must not become its parent in any way
//! that would let a GUI exit kill it, so we:
//!   - re-exec our own binary with `--daemon` (and the same `--config-dir`, so the
//!     spawned daemon shares the GUI's config-dir-isolated endpoint — dev and prod
//!     deliberately run separate daemons);
//!   - detach the child from the GUI's process group/session (`setsid()` on Unix;
//!     `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` creation flags on Windows);
//!   - give it no console of its own (std streams → the null device);
//!   - never `wait()` on it (it's meant to run forever).
//! Then we poll the endpoint until it's connectable, so the caller can immediately
//! proceed to connect.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::core::config;
use crate::daemon::{pidfile, transport};

/// How long to wait for a freshly spawned daemon to start listening before we
/// give up. Generous enough to cover a cold process start, short enough that a
/// genuinely-broken daemon surfaces as an error quickly rather than hanging the
/// GUI launch.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(3);
/// Poll interval while waiting for the socket to come up.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long to wait for the old daemon to exit after we ask it to shut down.
/// Generous on purpose: the daemon hangs up every pane's child (a ~200 ms SIGHUP
/// grace each) before it exits, so a session with several panes needs a moment.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(6);
/// How long a SIGTERMed daemon gets to finish its graceful teardown (same
/// per-pane SIGHUP grace as above) before we escalate to SIGKILL.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const REAP_TERM_TIMEOUT: Duration = Duration::from_secs(6);
/// How long a SIGKILLed daemon gets to disappear from the process table.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const REAP_KILL_TIMEOUT: Duration = Duration::from_secs(2);

/// Ensure a daemon is running for this process's config dir, spawning a detached
/// one if needed. Returns `Ok(())` once the endpoint is connectable; `Err` if the
/// endpoint can't be resolved or the daemon never came up within
/// [`STARTUP_TIMEOUT`].
pub fn ensure_running() -> anyhow::Result<()> {
    // Fast path: a live daemon answers `connect` immediately. We only want to
    // probe — drop the connection right away so we don't hold a pane open.
    if transport::connect().is_ok() {
        return Ok(());
    }

    // Nobody answers — but "unreachable" is not "gone". If the pidfile records
    // a daemon that is still alive (wedged, or one whose endpoint was lost),
    // its panes are already beyond reach; reap it before claiming the endpoint
    // so it can't linger forever holding every pane's PTY and children.
    reap_recorded_daemon();

    // If an endpoint marker is sitting there, it's a stale leftover from a
    // crashed daemon (a *live* one would have answered the connect above),
    // so clear it. The daemon's own `run()` clears stale endpoints too, but doing
    // it here means our post-spawn polling connects on the first try instead of
    // racing the daemon's cleanup.
    if transport::endpoint_exists() {
        transport::remove_stale_endpoint();
    }

    spawn_detached()?;

    // Wait for the daemon to bind + start accepting. We re-probe with `connect`
    // rather than just checking for the endpoint marker, since the marker appears
    // (via `bind`) slightly before the accept loop is ready.
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if transport::connect().is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "daemon did not start listening at {} within {:?}",
                transport::endpoint_display(),
                STARTUP_TIMEOUT
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Restart the daemon: ask the running one to shut down — which hangs up every
/// live shell — wait for it to exit, then spawn a fresh one. Returns once the new
/// daemon is listening.
///
/// The GUI exposes this as "Restart Background Service": a long-lived daemon
/// process keeps whatever environment it started with, so a change it can't pick
/// up live only takes effect on restart — a macOS permission granted after launch
/// (e.g. Full Disk Access), or an updated PATH / env on any platform — and
/// quitting/reopening the GUI alone doesn't touch the detached daemon. Safe with
/// no daemon running — it just spawns a fresh one.
pub fn restart() -> anyhow::Result<()> {
    stop();
    ensure_running()
}

/// Stop the running daemon and leave nothing running: ask it to shut down —
/// which hangs up every live shell — wait for it to exit, escalate to a
/// pid-based reap if it won't, and clear its endpoint marker. A no-op when no
/// daemon is running. Unlike [`restart`], this does not spawn a replacement.
///
/// This backs both the GUI's restart (which calls it, then respawns) and the
/// `--stop-daemon` CLI entry point the Windows installer/uninstaller runs before
/// replacing or deleting `tty7.exe`: the detached daemon is the running image of
/// that same file, so Windows locks it until the daemon exits. Stopping it here
/// releases the lock so the install/uninstall can overwrite/remove the binary.
pub fn stop() {
    use crate::daemon::protocol::ClientMsg;
    use std::io::Write as _;

    // Ask a running daemon to stop. Best effort: a failed connect/write means
    // nothing is listening, so we fall through to the reap/clear below.
    if let Ok(mut stream) = transport::connect() {
        if ClientMsg::Shutdown.encode(&mut stream).is_ok() {
            let _ = stream.flush();
            // The old daemon is gone once the endpoint stops answering (its
            // process exited and the listener closed). Poll until then, bounded.
            let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
            while Instant::now() < deadline && transport::connect().is_ok() {
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }

    // If the old daemon is still alive here, `Shutdown` didn't stop it — a
    // binary that predates the message, or a wedged teardown. Stopping *means*
    // the old daemon must go: quietly claiming its endpoint while it lives is
    // how sessions got stranded (unreachable daemon, panes and children still
    // running — issue #42). Escalate by recorded pid.
    reap_recorded_daemon();

    // The daemon removes its own endpoint marker on shutdown, but clear
    // defensively in case it was killed mid-teardown.
    if transport::endpoint_exists() {
        transport::remove_stale_endpoint();
    }
}

/// Reap the daemon recorded in the pidfile, if it is still alive: the caller
/// has decided that daemon must go (it stopped answering, or a restart was
/// ordered and `Shutdown` didn't stop it), and leaving it running while a new
/// daemon claims the endpoint would strand it — alive, unreachable, and
/// holding every pane's PTY and children.
///
/// Never trusts the pidfile blindly: the pid must still be alive *and* its
/// executable basename must match our own (the daemon is this same binary),
/// or the pid was recycled and the file is just stale — cleared, not killed.
/// Always ends with the pidfile removed; the daemon we spawn next writes its
/// own.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn reap_recorded_daemon() {
    let Some(pid) = pidfile::read() else { return };
    if pid <= 1 || pid == std::process::id() {
        // A pidfile naming init or ourselves is corrupt, not a daemon.
        pidfile::remove();
        return;
    }
    if process_matches_own_exe(pid as libc::pid_t) {
        log::warn!("reaping unreachable daemon (pid {pid}); its sessions will be hung up");
        reap_process(pid as libc::pid_t);
    }
    pidfile::remove();
}

/// Whether `pid` is alive and runs an executable with the same basename as our
/// own (GUI and daemon are the same `tty7` binary). This is the guard that
/// keeps a stale pidfile — daemon crashed, pid recycled by some unrelated
/// process — from getting an innocent process killed.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_matches_own_exe(pid: libc::pid_t) -> bool {
    let ours = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_os_string()));
    let theirs = process_path(pid).and_then(|p| p.file_name().map(|n| n.to_os_string()));
    matches!((ours, theirs), (Some(a), Some(b)) if a == b)
}

/// Terminate `pid` with escalation: SIGTERM first — a current daemon tears
/// down like `Shutdown`, giving every pane's child its SIGHUP grace (see
/// `server::serve_sigterm`) — then SIGKILL if it outlives the grace window.
/// Best effort: if it still won't die (unkillable, e.g. stuck in the kernel),
/// log and move on; the new daemon binds a fresh endpoint regardless.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn reap_process(pid: libc::pid_t) {
    if signal_and_await_exit(pid, libc::SIGTERM, REAP_TERM_TIMEOUT) {
        return;
    }
    if !signal_and_await_exit(pid, libc::SIGKILL, REAP_KILL_TIMEOUT) {
        log::error!("daemon pid {pid} survived SIGKILL; leaving it behind");
    }
}

/// Send `sig` to `pid` and poll until it exits or `timeout` elapses. Returns
/// whether the process is gone.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn signal_and_await_exit(pid: libc::pid_t, sig: libc::c_int, timeout: Duration) -> bool {
    // SAFETY: plain kill(2); a dead/foreign pid just returns an error.
    unsafe { libc::kill(pid, sig) };
    let deadline = Instant::now() + timeout;
    while process_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    true
}

/// Whether `pid` exists and is ours to signal (`kill(pid, 0)`). A pid held by
/// another user's process reads as "not alive" (EPERM) — correct for the reap
/// paths, which must then leave it alone.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 probes deliverability without delivering anything.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Windows reap: same contract as the Unix version, built on the `winproc`
/// process-table helpers the panes already use for hangup. There is no signal
/// to ask for a graceful teardown, so this mirrors `DaemonPane`'s Windows
/// hangup order instead: terminate the daemon's descendants deepest-first
/// (while their parent links are still live), then the daemon itself.
#[cfg(windows)]
fn reap_recorded_daemon() {
    use crate::daemon::winproc;

    let Some(pid) = pidfile::read() else { return };
    if pid <= 4 || pid == std::process::id() {
        // System idle/System pids or ourselves: corrupt, not a daemon.
        pidfile::remove();
        return;
    }
    let procs = winproc::snapshot();
    let ours = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    let matches = procs
        .iter()
        .find(|p| p.pid == pid)
        .zip(ours)
        .is_some_and(|(entry, name)| entry.name.eq_ignore_ascii_case(&name));
    if matches {
        log::warn!("reaping unreachable daemon (pid {pid}); its sessions will be hung up");
        for descendant in winproc::descendants(&procs, pid) {
            winproc::terminate(descendant);
        }
        winproc::terminate(pid);
    }
    pidfile::remove();
}

/// No process-table access on other platforms: the reap is a best-effort
/// rescue, so takeover there just keeps the pre-pidfile behavior.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn reap_recorded_daemon() {}

/// Re-exec our own binary as a detached `--daemon`, inheriting the resolved
/// config dir. The child is fully severed from the GUI: its own session/process
/// group (so a GUI quit can't signal it) and null std streams (no console).
fn spawn_detached() -> anyhow::Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("could not locate own executable: {e}"))?;

    let mut cmd = Command::new(exe);
    cmd.arg("--daemon");

    // Forward the *resolved* config dir so the daemon uses the same endpoint we
    // just probed. If nothing resolves we omit the flag and let the child apply
    // its own default resolution (env var / home dir).
    if let Some(dir) = config::config_dir_path() {
        cmd.arg("--config-dir").arg(dir);
    }

    if let Some(shell) = detect_parent_shell() {
        // The detached daemon's parent becomes launchd/systemd, so capture the
        // shell that launched the GUI before detaching and let the pane builder
        // prefer it over a stale `$SHELL` / passwd login-shell value.
        cmd.env(crate::daemon::DETECTED_SHELL_ENV, shell);
    }

    // A daemon has no controlling terminal or console: send all three std streams
    // to the null device so nothing inherits the GUI's handles.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    detach(&mut cmd);

    // Spawn and intentionally drop the handle without waiting: the daemon is a
    // long-lived process, not a child we reap. Dropping the `Child` doesn't kill
    // it (Rust never auto-kills on drop), and the detach above reparents it.
    match cmd.spawn() {
        Ok(_child) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("failed to spawn daemon process: {e}")),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn detect_parent_shell() -> Option<PathBuf> {
    process_path(unsafe { libc::getppid() }).filter(|path| is_supported_shell(path))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn detect_parent_shell() -> Option<PathBuf> {
    None
}

fn is_supported_shell(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "zsh" | "bash" | "fish" | "pwsh" | "powershell" | "powershell.exe" | "pwsh.exe"
    )
}

/// The executable path of an arbitrary live process, used both to recognize
/// the shell that launched the GUI and to verify a pidfile's pid is still a
/// tty7 daemon before reaping it.
#[cfg(target_os = "macos")]
fn process_path(pid: libc::pid_t) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: valid buffer, and `proc_pidpath` writes at most `buf.len()` bytes.
    let len =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    Some(PathBuf::from(
        String::from_utf8_lossy(&buf[..len as usize]).into_owned(),
    ))
}

#[cfg(target_os = "linux")]
fn process_path(pid: libc::pid_t) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// Detach the child into its own session/process group so a GUI teardown can't
/// take the daemon down with it.
#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    // `setsid()` in the child (post-fork, pre-exec) detaches it into a brand-new
    // session + process group. Without this the daemon stays in the GUI's process
    // group and a session teardown (GUI quit, terminal close) could take it down
    // with us — exactly what a persistent daemon must avoid.
    //
    // SAFETY: `pre_exec` runs in the forked child before `exec`. `setsid` is
    // async-signal-safe and we touch no shared state here, so this is sound.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Windows analogue of the Unix `setsid` detach. `DETACHED_PROCESS` severs the
/// child from the GUI's console, `CREATE_NEW_PROCESS_GROUP` puts it in its own
/// group (so a Ctrl-C / group signal to the GUI doesn't reach it), and
/// `CREATE_NO_WINDOW` stops a console window from flashing up for the headless
/// daemon. These are the raw `CreateProcess` flag values (no `windows-sys`
/// dependency needed for three constants).
#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

// The stale-endpoint assertion is Unix-socket specific (Windows uses a loopback
// port file with different semantics), so this test only runs on Unix.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    #[test]
    fn supported_shell_detection_matches_shell_basenames_only() {
        assert!(is_supported_shell(Path::new("/opt/homebrew/bin/fish")));
        assert!(is_supported_shell(Path::new("/bin/zsh")));
        assert!(is_supported_shell(Path::new("/usr/bin/bash")));
        assert!(!is_supported_shell(Path::new(
            "/Applications/kitty.app/kitty"
        )));
        assert!(!is_supported_shell(Path::new("/usr/bin/omp")));
    }

    /// The reap guard: a live process whose executable is *not* ours must never
    /// match — this is what keeps a stale pidfile with a recycled pid from
    /// getting an innocent process killed. Driven with a real `sleep` child:
    /// alive, path readable, basename `sleep` ≠ the test binary's.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn reap_guard_rejects_a_live_process_of_another_executable() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as libc::pid_t;

        assert!(process_alive(pid), "the sleep child is alive and ours");
        assert_eq!(
            process_path(pid).and_then(|p| p.file_name().map(|n| n.to_os_string())),
            Some("sleep".into()),
            "process_path resolves an arbitrary pid, not just our parent"
        );
        assert!(
            !process_matches_own_exe(pid),
            "sleep must not match the test binary; matching here would mean the reap could kill it"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// Escalation actually terminates a process that ignores the polite signal:
    /// `sleep` dies to the SIGTERM leg already, and the poll must observe the
    /// exit and report it. The child is reaped concurrently because a zombie
    /// still answers `kill(pid, 0)` — in production the daemon is launchd's
    /// child and vanishes on death, which is what the wait thread simulates.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn signal_and_await_exit_observes_the_death_it_caused() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as libc::pid_t;
        let reaper = std::thread::spawn(move || {
            let _ = child.wait();
        });

        assert!(
            signal_and_await_exit(pid, libc::SIGTERM, std::time::Duration::from_secs(5)),
            "the child must be seen exiting within the grace window"
        );
        assert!(!process_alive(pid), "and be gone afterwards");
        reaper.join().unwrap();
    }

    /// A dead pid reads as not-alive, so the reap paths treat its pidfile as
    /// stale and clear it without signalling anything.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn process_alive_is_false_once_the_process_is_gone() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as libc::pid_t;
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!process_alive(pid));
    }

    /// A stale socket file (one nothing is listening on) must be treated as "not
    /// running": connecting to it fails, which is our trigger to clean up + spawn.
    /// We assert the failure kind so the stale-cleanup branch stays exercised even
    /// without actually launching a process.
    #[test]
    fn connect_to_stale_socket_path_fails() {
        let dir = std::env::temp_dir().join(format!("tty7-spawn-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.sock");
        // No listener was ever bound here, so the file doesn't exist and connect
        // must fail (NotFound). If a leftover file existed with no listener it'd be
        // ConnectionRefused — both are non-`Ok`, which is all `ensure_running`
        // relies on to decide "spawn a fresh daemon".
        let err = UnixStream::connect(&path).unwrap_err();
        assert!(matches!(
            err.kind(),
            ErrorKind::NotFound | ErrorKind::ConnectionRefused
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

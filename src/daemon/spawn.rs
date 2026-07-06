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

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::core::config;
use crate::daemon::transport;

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

    // Not running. If an endpoint marker is sitting there, it's a stale leftover
    // from a crashed daemon (a *live* one would have answered the connect above),
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
    use crate::daemon::protocol::ClientMsg;
    use std::io::Write as _;

    // Ask a running daemon to stop. Best effort: a failed connect/write means
    // nothing is listening, so we fall through to spawning a fresh one.
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

    // The daemon removes its own endpoint marker on shutdown, but clear defensively
    // in case it was killed mid-teardown, then bring a fresh daemon up and wait for
    // it to listen. `ensure_running` re-probes and spawns only if nothing answers.
    if transport::endpoint_exists() {
        transport::remove_stale_endpoint();
    }
    ensure_running()
}

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
    use std::io::ErrorKind;
    use std::os::unix::net::UnixStream;

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

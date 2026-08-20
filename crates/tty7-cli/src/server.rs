use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::json;
use tty7_core::client::PaneClient;
use tty7_core::core::config;
use tty7_core::daemon::protocol::FEATURE_HANDOFF;
use tty7_core::daemon::spawn;

use crate::commands::{Outcome, Report};

const START_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const LOG_TAIL_LINES: usize = 40;

pub const SERVER_EXE_ENV: &str = "TTY7_SERVER_EXE";

fn report(human: impl Into<String>, json: serde_json::Value) -> Result<Outcome> {
    Ok(Outcome::Report(Report {
        human: human.into(),
        json,
    }))
}

fn running() -> bool {
    PaneClient::local().version().is_ok()
}

/// Every verb, lifecycle ones included, acts on the server named by
/// `$TTY7_CONFIG_DIR`: `spawn::stop` dials `transport::connect`, which derives
/// its endpoint from the config dir, and `start` passes that same dir to the
/// server it launches. There is nothing left to guard against here — the
/// endpoint these verbs reach and the one `tty7 status` reports on are one and
/// the same by construction.
pub fn start() -> Result<Outcome> {
    if running() {
        return report(
            "the server is already running",
            json!({ "started": false, "running": true }),
        );
    }
    // Nobody answered, so whatever is recorded or still holding the server
    // seat is a stranded process (#667) — left alone it would make the spawn
    // below stand down and time out with nothing to show for it.
    spawn::reap_stranded();
    let exe = server_exe()?;
    let mut cmd = Command::new(&exe);
    cmd.arg("--daemon");
    if let Some(dir) = config::config_dir_path() {
        cmd.arg("--config-dir").arg(dir);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not start {}: {e}", exe.display()))?;
    let pid = child.id();
    let deadline = Instant::now() + START_TIMEOUT;
    while !running() {
        if Instant::now() >= deadline {
            let fate = match child.try_wait() {
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    "it was still running and has been killed"
                }
                Ok(Some(status)) => {
                    if status.success() {
                        "it had already exited cleanly"
                    } else {
                        "it had already exited with an error"
                    }
                }
                Err(_) => "its state could not be checked, so it was left alone",
            };
            // A spawn that exited cleanly did so because it stood down
            // against a seat holder — without naming it, the message points
            // at nothing anyone can act on (#667).
            bail!(
                "{} (pid {pid}) did not open its endpoints within {START_TIMEOUT:?} — {fate}{}",
                exe.display(),
                spawn::seat_holder_note()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    report(
        format!("started {} (pid {pid})", exe.display()),
        json!({ "started": true, "pid": pid, "exe": exe.display().to_string() }),
    )
}

pub fn stop() -> Result<Outcome> {
    if !running() {
        // Answering nothing is not the same as being gone: a stranded server
        // (#667) still holds the seat, and stop is the verb people reach for
        // to clear it. Only a free seat means there is truly nothing to stop.
        let Some(stranded) = tty7_core::daemon::singleton::holder_pid() else {
            return report(
                "the server is not running",
                json!({ "stopped": false, "running": false }),
            );
        };
        spawn::stop();
        // Compared against the pid, not mere occupancy: a fresh daemon may
        // legitimately claim the freed seat in this very window, and it is
        // not the thing this command failed to stop.
        if tty7_core::daemon::singleton::holder_pid() == Some(stranded) {
            bail!(
                "the stranded server could not be reaped{}",
                spawn::seat_holder_note()
            );
        }
        return report(
            format!("stopped a stranded server (pid {stranded})"),
            json!({ "stopped": true, "stranded": true, "pid": stranded }),
        );
    }
    spawn::stop();
    if running() {
        bail!("the server did not shut down on request");
    }
    report("stopped", json!({ "stopped": true }))
}

pub fn restart(hard: bool) -> Result<Outcome> {
    let client = PaneClient::local();
    let Ok(old) = client.version() else {
        return start();
    };
    if !hard && old.has_feature(FEATURE_HANDOFF) {
        return restart_in_place(&client, &old);
    }
    // Either `--hard`, or a daemon that cannot replace itself in place —
    // Windows, or a build from before the handoff existed. Stopping is the
    // only restart that daemon has — and the report has to own that, because
    // the default restart's promise is sessions kept.
    spawn::stop();
    if running() {
        bail!("the server did not shut down on request");
    }
    let how = if hard {
        "stopped and started"
    } else {
        "stopped and started (this server cannot restart in place)"
    };
    match start()? {
        Outcome::Report(r) => report(
            format!("{how}; sessions ended; {}", r.human),
            json!({
                "restarted": true,
                "in_place": false,
                "sessions_kept": false,
                "start": r.json,
            }),
        ),
        other => Ok(other),
    }
}

/// The daemon execs the tty7-server binary found by [`server_exe`]: same pid,
/// same ptys, every session kept. The socket closing is the handoff being
/// taken; the proof it *worked* is the version endpoint answering with a new
/// `instance`, because that value is minted once per process image.
fn restart_in_place(
    client: &PaneClient,
    old: &tty7_core::daemon::protocol::DaemonVersion,
) -> Result<Outcome> {
    let exe = server_exe()?;
    client.hand_off(&exe).map_err(|e| {
        anyhow::anyhow!(
            "the server refused to restart in place and was left running: {e} — \
             `tty7 server restart --hard` stops and starts it instead; sessions end"
        )
    })?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(new) = client.version()
            && new.instance != old.instance
        {
            let human = if new.build == old.build {
                format!(
                    "restarted in place (build {}); sessions kept running",
                    new.build
                )
            } else {
                format!(
                    "restarted in place (build {} -> {}); sessions kept running",
                    old.build, new.build
                )
            };
            return report(
                human,
                json!({
                    "restarted": true,
                    "in_place": true,
                    "build": new.build,
                    "sessions_kept": true,
                }),
            );
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    if client.version().is_ok() {
        // Still the old instance answering: the handoff was accepted but the
        // exec never landed. The daemon is intact, and so are its sessions.
        bail!(
            "the server took the handoff but is still running its old image — \
             `tty7 server restart --hard` stops and starts it instead; sessions end"
        );
    }
    // The endpoint is silent, but the seat still tells "becoming the new
    // image" apart from "died": the singleton lock survives the exec, so a
    // held seat is the handed-off daemon still coming up, carrying every
    // session. `start` would grant it one second of grace and then reap it —
    // ending exactly the sessions this command promised to keep.
    if tty7_core::daemon::singleton::holder_pid().is_some() {
        bail!(
            "the server took the handoff but has not started answering within \
             {START_TIMEOUT:?} — it still holds the server seat, so its sessions may yet \
             survive; give it a moment and check `tty7 server status`"
        );
    }
    // The seat is free: it went down mid-handoff, and its sessions with it;
    // what is left to restore is a serving endpoint.
    match start()? {
        Outcome::Report(r) => report(
            format!(
                "the server went away during the handoff, ending its sessions; {}",
                r.human
            ),
            json!({
                "restarted": true,
                "in_place": false,
                "sessions_kept": false,
                "start": r.json,
            }),
        ),
        other => Ok(other),
    }
}

pub fn logs() -> Result<Outcome> {
    let Some(path) = config::config_path("tty7.log") else {
        bail!("no config directory, so no log file location");
    };
    let mut human = format!("{}\n", path.display());
    let mut lines: Vec<String> = Vec::new();
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            lines = contents
                .lines()
                .rev()
                .take(LOG_TAIL_LINES)
                .map(str::to_string)
                .collect();
            lines.reverse();
            for line in &lines {
                human.push_str(line);
                human.push('\n');
            }
        }
        Err(_) => {
            human.push_str("no log file yet — set TTY7_LOG=info before starting the server\n");
        }
    }
    report(
        human,
        json!({ "path": path.display().to_string(), "lines": lines }),
    )
}

fn server_exe() -> Result<PathBuf> {
    let name = server_exe_name();
    let own_dir = std::env::current_exe()
        .ok()
        .and_then(|own| own.parent().map(Path::to_path_buf));
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    resolve_server_exe(
        std::env::var_os(SERVER_EXE_ENV).filter(|v| !v.is_empty()).as_deref(),
        own_dir.as_deref(),
        &path_dirs,
        name,
        |p| p.is_file(),
    )
    .ok_or_else(|| {
        anyhow::anyhow!(
            "could not find {name} next to this binary or on PATH — install it, or point \
             {SERVER_EXE_ENV} at it"
        )
    })
}

fn server_exe_name() -> &'static str {
    if cfg!(windows) {
        "tty7-server.exe"
    } else {
        "tty7-server"
    }
}

/// Where the server binary is, in the order the three sources are trusted.
///
/// An explicit override wins outright and is not checked for existence: the
/// caller asked for that path, and a "not found" from the spawn names it, where
/// silently falling through to a *different* binary would not.
///
/// A sibling of this binary comes next — that is the shipped layout — and PATH
/// last. Both are held to the same test: the sibling used to be accepted on
/// `exists()`, so a directory named `tty7-server` shadowed the real one on PATH
/// and turned a working install into a spawn failure.
///
/// `is_exe` is passed in so a test can answer without touching the filesystem.
fn resolve_server_exe(
    explicit: Option<&std::ffi::OsStr>,
    own_dir: Option<&Path>,
    path_dirs: &[PathBuf],
    name: &str,
    is_exe: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(explicit) = explicit {
        return Some(PathBuf::from(explicit));
    }
    if let Some(dir) = own_dir {
        let sibling = dir.join(name);
        if is_exe(&sibling) {
            return Some(sibling);
        }
    }
    path_dirs
        .iter()
        .map(|dir| dir.join(name))
        .find(|candidate| is_exe(candidate))
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt as _;

    // The daemon's flags, not a second opinion. `CREATE_NEW_PROCESS_GROUP` used
    // to be in here too, and it disables Ctrl+C for everything in the new group:
    // the server, every pane shell it spawns, and everything those shells run
    // (#451, #314). `DETACHED_PROCESS` alone already leaves the server without a
    // console for a control event to arrive on.
    cmd.creation_flags(spawn::DAEMON_CREATION_FLAGS);
}

#[cfg(not(any(unix, windows)))]
fn detach(_cmd: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    const NAME: &str = "tty7-server";

    fn dirs(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    /// Stands in for the filesystem: only the listed paths are runnable files.
    fn only(files: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |p: &Path| files.iter().any(|f| Path::new(f) == p)
    }

    #[test]
    fn the_override_wins_and_is_taken_at_its_word() {
        let got = resolve_server_exe(
            Some(OsStr::new("/opt/custom/tty7-server")),
            Some(Path::new("/usr/local/bin")),
            &dirs(&["/usr/bin"]),
            NAME,
            only(&["/usr/local/bin/tty7-server", "/usr/bin/tty7-server"]),
        );
        assert_eq!(got, Some(PathBuf::from("/opt/custom/tty7-server")));
    }

    /// Not existence-checked on purpose: a bad override should fail loudly by
    /// that name, not quietly run a different binary.
    #[test]
    fn a_nonexistent_override_is_still_returned() {
        let got = resolve_server_exe(
            Some(OsStr::new("/nope/tty7-server")),
            Some(Path::new("/usr/local/bin")),
            &dirs(&["/usr/bin"]),
            NAME,
            only(&["/usr/bin/tty7-server"]),
        );
        assert_eq!(got, Some(PathBuf::from("/nope/tty7-server")));
    }

    #[test]
    fn a_sibling_beats_path() {
        let got = resolve_server_exe(
            None,
            Some(Path::new("/opt/tty7")),
            &dirs(&["/usr/bin"]),
            NAME,
            only(&["/opt/tty7/tty7-server", "/usr/bin/tty7-server"]),
        );
        assert_eq!(got, Some(PathBuf::from("/opt/tty7/tty7-server")));
    }

    #[test]
    fn path_is_searched_in_order_when_there_is_no_sibling() {
        let got = resolve_server_exe(
            None,
            Some(Path::new("/opt/tty7")),
            &dirs(&["/a", "/b", "/c"]),
            NAME,
            only(&["/b/tty7-server", "/c/tty7-server"]),
        );
        assert_eq!(got, Some(PathBuf::from("/b/tty7-server")));
    }

    /// The bug behind holding both candidates to `is_file`: a *directory*
    /// named `tty7-server` beside this binary used to satisfy `exists()`, so it
    /// shadowed the real one on PATH and the spawn failed on a working install.
    #[test]
    fn a_directory_by_that_name_does_not_shadow_the_real_binary() {
        let got = resolve_server_exe(
            None,
            Some(Path::new("/opt/tty7")),
            &dirs(&["/usr/bin"]),
            NAME,
            // /opt/tty7/tty7-server exists but is not a file, so it is absent here.
            only(&["/usr/bin/tty7-server"]),
        );
        assert_eq!(got, Some(PathBuf::from("/usr/bin/tty7-server")));
    }

    #[test]
    fn nothing_anywhere_is_a_miss_rather_than_a_guess() {
        assert_eq!(
            resolve_server_exe(
                None,
                Some(Path::new("/opt/tty7")),
                &dirs(&["/usr/bin"]),
                NAME,
                only(&[]),
            ),
            None
        );
    }

    #[test]
    fn no_own_directory_falls_through_to_path() {
        let got = resolve_server_exe(
            None,
            None,
            &dirs(&["/usr/bin"]),
            NAME,
            only(&["/usr/bin/tty7-server"]),
        );
        assert_eq!(got, Some(PathBuf::from("/usr/bin/tty7-server")));
    }
}

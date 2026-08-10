use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::json;
use tty7_core::client::PaneClient;
use tty7_core::core::config;
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
            bail!(
                "{} (pid {pid}) did not open its endpoints within {START_TIMEOUT:?} — {fate}",
                exe.display()
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
        return report(
            "the server is not running",
            json!({ "stopped": false, "running": false }),
        );
    }
    spawn::stop();
    if running() {
        bail!("the server did not shut down on request");
    }
    report("stopped", json!({ "stopped": true }))
}

pub fn restart() -> Result<Outcome> {
    if running() {
        spawn::stop();
        if running() {
            bail!("the server did not shut down on request");
        }
    }
    start()
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
    if let Some(explicit) = std::env::var_os(SERVER_EXE_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(explicit));
    }
    let name = if cfg!(windows) {
        "tty7-server.exe"
    } else {
        "tty7-server"
    };
    if let Ok(own) = std::env::current_exe() {
        if let Some(dir) = own.parent() {
            let sibling = dir.join(name);
            if sibling.exists() {
                return Ok(sibling);
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!(
        "could not find {name} next to this binary or on PATH — install it, or point \
         {SERVER_EXE_ENV} at it"
    )
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

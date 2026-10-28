//! The mobile gateway, as a child of the daemon.
//!
//! Phones reach the panes this daemon holds, so this daemon is what keeps
//! the gateway running: while `Config::mobile_access` is on, it runs one as a
//! child process, and stops it when the switch goes off. Every daemon does
//! this the same way, whoever started it.
//!
//! The gateway is a process of its own, never a thread in here. iroh and its
//! runtime stay out of the process holding every pane, and a gateway that
//! falls over takes no session with it. Which program runs it depends on the
//! daemon:
//!
//! - `tty59-app --daemon` runs itself, as `tty59-app --mobile-gateway`;
//! - the lean `tty59-server` runs a `tty59-gateway` beside it or on `PATH`.
//!   It links nothing of the gateway, and a machine without one says so in
//!   the status file rather than seeming to start forever.
//!
//! The child's stdin is a pipe from here, and the gateway exits when that
//! pipe closes. So however this process ends — stopped, or crashed — its
//! gateway ends with it. A handoff stops it explicitly before the exec
//! ([`stop_for_handoff`]): the new image would never reap a child it did not
//! start. The next daemon starts a fresh one from its own binary, and no
//! gateway from an older build is left running.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::core::config::{Config, config_dir_path, config_path};

/// The flag `tty59-app` runs the gateway under.
pub const GATEWAY_FLAG: &str = "--mobile-gateway";
/// The flag `tty59-gateway serve` takes to live exactly as long as its stdin.
pub const EXIT_WITH_STDIN_FLAG: &str = "--exit-with-stdin";

const STATUS_FILE: &str = "status.json";
/// How often the daemon looks at the switch and at its gateway.
const POLL: Duration = Duration::from_secs(2);
/// How long after a gateway fails before the next is started.
const RETRY: Duration = Duration::from_secs(30);
/// How long a gateway asked to stop gets before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// The running gateway, where the supervisor and a handoff can both reach it.
static GATEWAY: Mutex<Option<Child>> = Mutex::new(None);
/// Set by a handoff: from here to the exec, no new gateway is started.
static HANDING_OFF: AtomicBool = AtomicBool::new(false);

fn gateway() -> std::sync::MutexGuard<'static, Option<Child>> {
    GATEWAY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Stops the gateway and waits for it, before this process becomes another
/// program. The new image supervises afresh and starts its own.
pub fn stop_for_handoff() {
    HANDING_OFF.store(true, Ordering::SeqCst);
    if let Some(child) = gateway().take() {
        stop(child);
    }
}

/// A handoff that did not happen: this daemon carries on, and its supervisor
/// starts the gateway again.
pub fn handoff_failed() {
    HANDING_OFF.store(false, Ordering::SeqCst);
}

/// The gateway's state directory: `<config dir>/mobile/`.
pub fn state_dir() -> Option<PathBuf> {
    Some(config_dir_path()?.join("mobile"))
}

/// How the gateway last said it was doing, in `mobile/status.json`, for the
/// Settings page to show. A gateway that dies without a word leaves
/// `Running` behind, so a reader also checks whether its lock is still held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Running { id: String, since: u64 },
    Stopped,
    Failed { error: String },
}

impl Status {
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join(STATUS_FILE)
    }
}

/// Starts the thread that keeps a gateway running exactly while mobile
/// access is on. Called once by every daemon.
pub fn supervise() {
    let spawned = std::thread::Builder::new()
        .name("mobile-supervisor".into())
        .spawn(|| {
            let mut switch = Switch::default();
            let mut next_try = Instant::now();
            let mut noted: Option<String> = None;
            loop {
                let exited = {
                    let mut running = gateway();
                    let exit = running.as_mut().and_then(|c| c.try_wait().ok().flatten());
                    if exit.is_some() {
                        *running = None;
                    }
                    exit
                };
                if let Some(exit) = exited {
                    next_try = Instant::now() + RETRY;
                    note(&mut noted, format!("the mobile gateway exited ({exit})"));
                }
                let running = gateway().is_some();
                if !switch.on() {
                    let child = gateway().take();
                    if let Some(child) = child {
                        stop(child);
                    }
                    next_try = Instant::now();
                    noted = None;
                } else if !running
                    && Instant::now() >= next_try
                    && !HANDING_OFF.load(Ordering::SeqCst)
                {
                    match start() {
                        Ok(child) => *gateway() = Some(child),
                        Err(why) => {
                            next_try = Instant::now() + RETRY;
                            if noted.as_deref() != Some(&why) {
                                write_failed(&why);
                            }
                            note(&mut noted, why);
                        }
                    }
                }
                std::thread::sleep(POLL);
            }
        });
    if let Err(e) = spawned {
        log::error!("could not start the mobile supervisor: {e}");
    }
}

/// Logs a problem the first time it is seen, not every poll.
fn note(noted: &mut Option<String>, why: String) {
    if noted.as_deref() != Some(&why) {
        log::warn!("{why}");
        *noted = Some(why);
    }
}

fn start() -> Result<Child, String> {
    let mut cmd = gateway_command().ok_or_else(|| {
        "this tty59 server cannot run the mobile gateway: there is no tty59-gateway \
         beside it or on PATH"
            .to_string()
    })?;
    if let Some(dir) = config_dir_path() {
        cmd.env("TTY59_CONFIG_DIR", dir);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::daemon::spawn::detach_child(&mut cmd);
    cmd.spawn()
        .map_err(|e| format!("could not start the mobile gateway: {e}"))
}

/// Closes the gateway's stdin, which is its cue to stop, and kills it if it
/// has not gone within the grace period.
fn stop(mut child: Child) {
    drop(child.stdin.take());
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The program that runs the gateway for this daemon, if it has one.
fn gateway_command() -> Option<Command> {
    let exe = std::env::current_exe().ok()?;
    if exe.file_stem().and_then(|s| s.to_str()) == Some("tty59-app") {
        let mut cmd = Command::new(&exe);
        cmd.arg(GATEWAY_FLAG);
        return Some(cmd);
    }
    let name = format!("tty59-gateway{}", std::env::consts::EXE_SUFFIX);
    let beside = exe.with_file_name(&name);
    let program = if beside.is_file() {
        beside
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(&name))
            .find(|path| path.is_file())?
    };
    let mut cmd = Command::new(program);
    cmd.args(["serve", EXIT_WITH_STDIN_FLAG]);
    Some(cmd)
}

/// Says why no gateway is running, where Settings will look. Written only
/// by the daemon, and only when it could not start one: once a gateway runs,
/// the file is its own.
fn write_failed(why: &str) {
    let Some(dir) = state_dir() else {
        return;
    };
    let status = Status::Failed {
        error: why.to_string(),
    };
    let Ok(json) = serde_json::to_vec_pretty(&status) else {
        return;
    };
    let path = Status::path_in(&dir);
    let tmp = path.with_extension("tmp");
    let _ = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&tmp, json))
        .and_then(|()| std::fs::rename(&tmp, &path));
}

/// `mobile_access` as the config file says, re-read only when the file
/// changes: the daemon asks every couple of seconds for as long as it lives.
#[derive(Default)]
struct Switch {
    seen: Option<SystemTime>,
    on: bool,
}

impl Switch {
    fn on(&mut self) -> bool {
        let modified = config_path("config.json")
            .and_then(|file| std::fs::metadata(file).ok()?.modified().ok());
        if modified.is_none() || modified != self.seen {
            self.seen = modified;
            self.on = Config::load().mobile_access;
        }
        self.on
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_file_keeps_its_shape() {
        // Written here and by tty59-gateway, read by the GUI: one shape.
        let json = serde_json::to_value(Status::Failed { error: "x".into() }).unwrap();
        assert_eq!(json, serde_json::json!({"state": "failed", "error": "x"}));
        let json = serde_json::to_value(Status::Running {
            id: "k".into(),
            since: 1,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"state": "running", "id": "k", "since": 1})
        );
    }
}

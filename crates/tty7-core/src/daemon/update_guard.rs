//! A file that says "an installer is replacing this installation right now".
//!
//! Between `spawn::stop_for_update` clearing the installed images and the
//! installer finishing, nothing used to stop a `tty7` CLI call — or a
//! manually launched GUI — from spawning a fresh daemon that relocks the very
//! files being replaced. The guard closes that window: whoever drives the
//! installation holds it, and `spawn::ensure_running` refuses to spawn a
//! daemon while it is held. Only spawning is deferred; connecting to a daemon
//! that is already running stays untouched.
//!
//! The guard names its holder by pid, and a pid is only believed to be the
//! holder while the process behind it *could* be: it must be alive, and it
//! must have started before the guard was written — a process born later
//! merely inherited the number. That check is what lets a holder keep the
//! guard for as long as its installation genuinely runs (an install slowed
//! past any fixed budget by an antivirus sweep stays protected), while a
//! crashed holder's guard goes stale the moment its pid dies or is recycled.
//! Only when the start time cannot be read at all does a TTL bound the doubt.
//!
//! Two kinds of holder:
//! - the auto-updater (`tty7-updater.exe install`/`install-portable`) holds
//!   for itself, from stopping the daemon until it relaunches the app;
//! - the `--stop-daemon --update-install-dir` helper that Inno's
//!   `PrepareToInstall`/`[UninstallRun]` runs holds for its *parent* — the
//!   Setup or uninstaller that keeps replacing files long after the helper
//!   returns. Nobody clears that one; it goes stale when Setup exits.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::core::config;
use crate::daemon::winproc;

/// Bounds how long a writer whose start time cannot be read may hold spawns
/// back. Never reached by a verifiable writer — see [`writer_holds`].
const GUARD_TTL: Duration = Duration::from_secs(10 * 60);

/// Clock-versus-filesystem slack when comparing a process's start against the
/// guard's mtime. Generous: the two are the same machine's clock, but FAT
/// timestamps are coarse.
const START_SLACK: Duration = Duration::from_secs(10);

fn path() -> Option<PathBuf> {
    config::config_path("update.lock")
}

/// Claims the guard for the calling process.
pub fn hold() {
    hold_for(std::process::id());
}

/// Claims the guard for the calling process's parent. For the helper Inno
/// runs: the helper exits as soon as the daemon stop returns, but its parent
/// — Setup — lives exactly as long as the files are being replaced, which is
/// the lifetime the guard has to match. Falls back to the caller itself when
/// the parent cannot be named; that guard goes stale at the caller's exit,
/// which is no worse than not holding one.
pub fn hold_for_parent() {
    let own = std::process::id();
    let parent = winproc::snapshot()
        .iter()
        .find(|process| process.pid == own)
        .map(|process| process.parent);
    match parent {
        Some(pid) if pid > 4 => hold_for(pid),
        _ => hold(),
    }
}

fn hold_for(pid: u32) {
    let Some(path) = path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = std::fs::write(&path, pid.to_string()) {
        log::warn!(
            "could not write the update guard {}: {error}",
            path.display()
        );
    }
}

/// Releases the guard. Harmless when it is not held.
pub fn clear() {
    if let Some(path) = path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether an installer is replacing the installation right now. A stale
/// guard — dead or recycled writer, unreadable garbage — is removed on sight,
/// so one crashed holder never costs more than one look.
pub(crate) fn held() -> bool {
    let Some(path) = path() else { return false };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return false;
    };
    let written = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .ok();
    let holds =
        contents.trim().parse::<u32>().ok().is_some_and(|pid| {
            writer_holds(process_alive(pid), winproc::creation_time(pid), written)
        });
    if !holds {
        log::info!("removing a stale update guard at {}", path.display());
        let _ = std::fs::remove_file(&path);
    }
    holds
}

/// The staleness policy, pure so every case is testable: `started` is when
/// the process wearing the recorded pid began, `written` the guard's mtime.
fn writer_holds(alive: bool, started: Option<SystemTime>, written: Option<SystemTime>) -> bool {
    if !alive {
        return false;
    }
    match (started, written) {
        // The writer wrote the guard after it started; a "writer" born later
        // is a recycled pid wearing its number. A verified writer holds for
        // as long as it lives — an install slowed past any fixed budget is
        // still an install.
        (Some(started), Some(written)) => started <= written + START_SLACK,
        // Alive but unverifiable: the TTL bounds how long a pid that cannot
        // be told from a recycled one may hold spawns back.
        (None, Some(written)) => written.elapsed().is_ok_and(|age| age <= GUARD_TTL),
        // No readable mtime to reason from at all.
        _ => false,
    }
}

fn process_alive(pid: u32) -> bool {
    !winproc::wait_for_exit(pid, Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin_config_dir() {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        config::set_config_dir(dir);
    }

    fn exited_pid() -> u32 {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn staleness_policy_trusts_only_a_live_writer_born_before_the_guard() {
        let now = SystemTime::now();
        let before = now - Duration::from_secs(60);
        let later = now + Duration::from_secs(60);

        assert!(writer_holds(true, Some(before), Some(now)));
        assert!(
            writer_holds(true, Some(now - GUARD_TTL * 3), Some(now - GUARD_TTL * 2)),
            "a verified writer is never expired by the TTL, however long it runs"
        );
        assert!(
            !writer_holds(true, Some(later), Some(now)),
            "a process born after the guard is a recycled pid"
        );
        assert!(!writer_holds(false, Some(before), Some(now)));

        // Liveness without identity gets exactly the TTL.
        assert!(writer_holds(true, None, Some(now)));
        assert!(!writer_holds(true, None, Some(now - GUARD_TTL * 2)));
        assert!(!writer_holds(true, None, None));
    }

    // One test for the file lifecycle, like the pidfile's: the guard file is
    // process-global state, and two tests sharing it would race each other.
    #[test]
    fn guard_lifecycle_holds_for_a_live_writer_and_sheds_stale_files() {
        pin_config_dir();
        clear();
        assert!(!held(), "no guard file, no guard");

        hold();
        assert!(held(), "this process is alive and older than its guard");
        clear();
        assert!(!held());

        std::fs::write(path().unwrap(), exited_pid().to_string()).unwrap();
        assert!(!held(), "a dead writer cannot be installing anything");
        assert!(
            !path().unwrap().exists(),
            "the stale guard is gone after one look"
        );

        // Garbage is stale by the same rule.
        std::fs::write(path().unwrap(), "not-a-pid").unwrap();
        assert!(!held());
        assert!(!path().unwrap().exists());
    }
}

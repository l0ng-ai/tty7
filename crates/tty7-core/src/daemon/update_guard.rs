//! A file that says "an updater is replacing this installation right now".
//!
//! Between `spawn::stop_for_update` clearing the installed images and the
//! installer finishing, nothing used to stop a `tty7` CLI call — or a
//! manually launched GUI — from spawning a fresh daemon that relocks the very
//! files being replaced. The guard closes that window: the updater holds it
//! for the whole installation and releases it just before relaunching the
//! app, and `spawn::ensure_running` refuses to spawn a daemon while it is
//! held. Only spawning is deferred; connecting to a daemon that is already
//! running stays untouched.
//!
//! The guard names its holder by pid so it can never outlive a crashed
//! updater: a guard whose writer is gone is removed on sight, and a TTL
//! backstops the one coincidence pid-liveness cannot see — the dead writer's
//! pid recycled by an unrelated long-lived process.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::core::config;

/// Far above any real installation's duration (Setup runs in seconds), and
/// the most a recycled writer pid can cost.
const GUARD_TTL: Duration = Duration::from_secs(10 * 60);

fn path() -> Option<PathBuf> {
    config::config_path("update.lock")
}

/// Claims the guard for the calling process.
pub fn hold() {
    let Some(path) = path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = std::fs::write(&path, std::process::id().to_string()) {
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

/// Whether an updater is installing right now. A guard whose writer has
/// exited or that outlived the TTL is stale — the updater clears it on every
/// path that relaunches the app, so a leftover means it died — and is removed
/// here so one crash never costs more than one look.
pub(crate) fn held() -> bool {
    let Some(path) = path() else { return false };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return false;
    };
    let live = contents
        .trim()
        .parse::<u32>()
        .is_ok_and(|pid| process_alive(pid));
    if !live || expired(&path) {
        log::info!("removing a stale update guard at {}", path.display());
        let _ = std::fs::remove_file(&path);
        return false;
    }
    true
}

fn expired(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > GUARD_TTL)
}

fn process_alive(pid: u32) -> bool {
    !crate::daemon::winproc::wait_for_exit(pid, Duration::ZERO)
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

    // One test, like the pidfile's: the guard file is process-global state,
    // and two tests sharing it would race each other.
    #[test]
    fn guard_lifecycle_holds_for_a_live_writer_and_sheds_stale_files() {
        pin_config_dir();
        clear();
        assert!(!held(), "no guard file, no guard");

        hold();
        assert!(held(), "this process is alive, so its guard holds");
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

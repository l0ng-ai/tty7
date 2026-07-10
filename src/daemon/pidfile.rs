//! The daemon's pid marker: `<config>/daemon.pid`, written after a successful
//! `bind` and removed on shutdown.
//!
//! The endpoint marker (socket / port file) answers "is something listening
//! *here*?", but says nothing about *which process* — and that gap is exactly
//! how daemons got stranded (see the takeover paths in `spawn`): a client that
//! couldn't talk to the old daemon would unlink its endpoint and start a fresh
//! one, leaving the old process alive, unreachable, and still holding every
//! pane's PTY + children. The pidfile closes the gap: takeover paths read it
//! and reap the recorded process before claiming the endpoint.
//!
//! A pidfile can outlive its daemon (crash, SIGKILL), and pids get recycled —
//! so readers must never trust it blindly. `spawn::reap_recorded_daemon`
//! verifies the pid's executable basename matches our own before signalling.

use std::path::PathBuf;

use crate::core::config;

/// Path of the pidfile for this process's config dir. `None` only when the
/// config dir can't be resolved (no `$HOME`).
pub fn path() -> Option<PathBuf> {
    config::config_path("daemon.pid")
}

/// Record the current process as the daemon serving this config dir. Best
/// effort: the pidfile is a rescue marker, not a correctness requirement, so a
/// failed write must not take the daemon down — it just means a future
/// takeover can't reap us and falls back to today's behavior.
pub fn write_current() {
    let Some(path) = path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, std::process::id().to_string()) {
        log::warn!("could not write pidfile {}: {e}", path.display());
    }
}

/// The recorded daemon pid, if the pidfile exists and parses. Says nothing
/// about whether that process is still alive or still a tty7 daemon.
pub fn read() -> Option<u32> {
    let contents = std::fs::read_to_string(path()?).ok()?;
    contents.trim().parse::<u32>().ok()
}

/// Remove the pidfile. Best effort: a missing file is fine.
pub fn remove() {
    if let Some(path) = path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the process config dir so the pidfile lives under a temp dir, never
    /// the real `~/.config`. First-call-wins across the whole test binary, so
    /// use the same directory the other IO tests pin.
    fn pin_config_dir() {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        config::set_config_dir(dir);
    }

    /// One test drives the whole lifecycle — write → read → remove → reject
    /// garbage — so the shared `daemon.pid` file isn't raced by parallel tests
    /// (same reason transport's endpoint test is a single lifecycle).
    #[test]
    fn pidfile_lifecycle_round_trips_clears_and_rejects_garbage() {
        pin_config_dir();
        write_current();
        assert_eq!(read(), Some(std::process::id()));
        remove();
        assert_eq!(read(), None, "no pid after removal");
        // Removing again is harmless.
        remove();

        // A corrupt file (partial write, hand-edited) must read as "no pid",
        // never panic or misparse.
        std::fs::write(path().unwrap(), "not-a-pid\n").unwrap();
        assert_eq!(read(), None);
        remove();
    }
}

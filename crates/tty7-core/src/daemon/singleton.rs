//! One server per config directory, decided by the kernel.
//!
//! Two servers against one config dir is not a degraded mode, it is a silent
//! one. They do not fight over both endpoints — they split them. The one that
//! keeps `control.sock` answers `MachineGet` out of its own pane registry,
//! which is empty because the panes are all in the *other* process, so every
//! pane in the machine tree reads as dead. Nothing logs an error; the window
//! just quietly rebuilds every session it was asked to restore.
//!
//! That split was reachable because "is a server already running?" was answered
//! by connecting to its socket and treating one failed connect as proof of
//! death — after which the newcomer unlinked the endpoint and bound its own.
//! A connect can fail for a server that is merely slow to reach `accept`, and
//! the loser of that race never learns it was replaced: it holds a listener on
//! an unlinked path and serves nobody, forever.
//!
//! An advisory lock has no such gap. Holding it is the definition of being the
//! server, the kernel drops it when the holder dies however it dies, and there
//! is no stale state to interpret — which is the property the socket probe
//! could never have.
//!
//! Take it *before* either endpoint. Then a stale endpoint can only ever be
//! removed by a process that already knows it is alone, and the removal is safe
//! by construction rather than by timing.

use std::fs::File;
use std::path::PathBuf;

/// Proof that this process is the server for its config directory.
///
/// Released when dropped, and by the kernel if the process never gets to drop
/// it. Hold it for as long as the server serves.
#[derive(Debug)]
pub struct Singleton {
    _file: File,
}

/// The outcome of asking to be the server.
#[derive(Debug)]
pub enum Claim {
    /// This process holds it. Serve.
    Held(Singleton),
    /// Another live server holds it. Stand down and talk to that one.
    Taken,
    /// The lock could not be evaluated (no config dir, unwritable path). The
    /// caller carries on unprotected rather than refusing to start: a server
    /// that will not run is worse than one that might race.
    Unavailable(String),
}

fn lock_path() -> Option<PathBuf> {
    crate::core::config::config_path("daemon.lock")
}

/// The descriptor the seat is held on, for the one caller that needs it after
/// the fact: a handoff, which has to pass it to the image it is becoming.
///
/// Recorded rather than threaded through because the seat is deliberately owned
/// by `run_daemon`'s stack frame — it is released when the server stops serving,
/// and that is the property worth keeping. `-1` means no seat is held.
#[cfg(unix)]
static HELD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

#[cfg(unix)]
pub fn held_fd() -> Option<std::os::fd::RawFd> {
    match HELD.load(std::sync::atomic::Ordering::Relaxed) {
        -1 => None,
        fd => Some(fd),
    }
}

#[cfg(unix)]
fn note_held(file: &File) {
    HELD.store(
        std::os::fd::AsRawFd::as_raw_fd(file),
        std::sync::atomic::Ordering::Relaxed,
    );
}

#[cfg(not(unix))]
fn note_held(_file: &File) {}

/// Take back a seat this process never gave up.
///
/// After `execve` the lock is still held — by this process, which is the same
/// process, wearing a new program. Calling [`claim`] here would open the file a
/// second time and ask the kernel for a lock the first descriptor still has, be
/// told `Taken`, and stand down in favour of itself: the one way to end up with
/// no daemon at all is to be too careful here.
///
/// # Safety
///
/// `fd` must be an open descriptor on the lock file, inherited across the exec
/// and not owned by anything else.
#[cfg(unix)]
pub unsafe fn adopt(fd: std::os::fd::RawFd) -> Singleton {
    // The exec this descriptor crossed required stripping close-on-exec; put it
    // back before anything is spawned. A child inheriting the seat holds the
    // flock as long as it lives — an ssh transport that outlives a killed
    // daemon would then keep every future daemon standing down in favour of a
    // process that is not serving anyone.
    if let Err(e) = crate::daemon::handoff::close_on_exec_again(fd) {
        log::warn!("could not restore close-on-exec on the seat descriptor {fd}: {e}");
    }
    let file = unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    note_held(&file);
    Singleton { _file: file }
}

/// Claims the right to be this machine's server.
pub fn claim() -> Claim {
    let Some(path) = lock_path() else {
        return Claim::Unavailable("no config directory".into());
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match open_exclusive(&path) {
        Ok(Some(file)) => {
            note_held(&file);
            Claim::Held(Singleton { _file: file })
        }
        Ok(None) => Claim::Taken,
        Err(e) => Claim::Unavailable(format!("{} could not be locked: {e}", path.display())),
    }
}

/// `Ok(Some(file))` when the lock is ours, `Ok(None)` when someone else holds
/// it, `Err` when the question could not be put to the kernel at all.
#[cfg(unix)]
fn open_exclusive(path: &std::path::Path) -> std::io::Result<Option<File>> {
    use std::os::unix::io::AsRawFd as _;

    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    loop {
        // LOCK_NB so a running server answers "taken" instead of parking this
        // process on a lock it will hold until it exits.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(file));
        }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EWOULDBLOCK) => return Ok(None),
            // A signal arriving mid-call says nothing about the lock. Reporting
            // it as "could not be evaluated" would start a second server beside
            // the first — the split machine this module exists to prevent —
            // for no better reason than a `SIGCHLD` landing at the wrong
            // microsecond.
            Some(libc::EINTR) => continue,
            _ => return Err(e),
        }
    }
}

#[cfg(windows)]
fn open_exclusive(path: &std::path::Path) -> std::io::Result<Option<File>> {
    use std::os::windows::fs::OpenOptionsExt as _;

    // share_mode(0) is the whole mechanism: the file stays open for as long as
    // the server runs, and every other open of it fails until this handle is
    // closed — by `drop`, or by Windows when the process ends.
    match File::options()
        .create(true)
        .read(true)
        .write(true)
        .share_mode(0)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.raw_os_error() == Some(32) => Ok(None), // ERROR_SHARING_VIOLATION
        Err(e) => Err(e),
    }
}

#[cfg(not(any(unix, windows)))]
fn open_exclusive(_path: &std::path::Path) -> std::io::Result<Option<File>> {
    // No lock primitive here. `Ok(None)` would read as "taken" and stop the
    // server from ever starting; not being able to ask is `Unavailable`.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no single-server lock on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_config_dir` is process-wide, so these cases cannot run beside each
    /// other: one would be claiming the very seat the other is asserting is
    /// free, and which failed would depend on the scheduler.
    static CONFIG_DIR: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn pin_dir(name: &str) -> (PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = CONFIG_DIR.lock().unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("tty7-singleton-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir);
        // `set_config_dir` is first-wins, so the directory in force may be one
        // an earlier test in this process pinned. Hand back the one the seat
        // will actually be taken in — a case that writes to the directory it
        // asked for instead would be testing a file nothing ever reads.
        let path = lock_path().expect("a config directory is pinned");
        let dir = path.parent().expect("the lock lives in a directory");
        (dir.to_path_buf(), guard)
    }

    /// [`claim`], allowing for a seat some neighbour is still holding a
    /// reference to.
    ///
    /// A `Command::spawn` anywhere in this process forks every descriptor this
    /// one has open, and BSD `flock` counts an inherited descriptor as another
    /// reference to the same lock rather than a second lock — so between a
    /// neighbour's `fork` and its `exec`, a seat this thread has already
    /// dropped still reads as taken. The suite forks constantly. What these
    /// cases are about is that the seat comes back, not the microsecond it
    /// comes back in.
    fn claim_within(patience: std::time::Duration) -> Claim {
        let deadline = std::time::Instant::now() + patience;
        loop {
            match claim() {
                Claim::Taken if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                other => return other,
            }
        }
    }

    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

    #[test]
    fn the_second_claim_is_refused_while_the_first_is_held() {
        let _pinned = pin_dir("basic");
        let first = match claim_within(PATIENCE) {
            Claim::Held(s) => s,
            other => panic!("the first claim must be granted, got {other:?}"),
        };
        assert!(
            matches!(claim(), Claim::Taken),
            "a second server must be told the seat is taken, not race for the endpoints"
        );
        drop(first);
        assert!(
            matches!(claim_within(PATIENCE), Claim::Held(_)),
            "releasing it hands the seat to the next server — no stale file to interpret"
        );
    }

    /// A neighbour that forks while the seat is held keeps the seat alive past
    /// the holder's `drop`, and this suite forks constantly.
    #[cfg(unix)]
    #[test]
    fn a_reference_a_forking_neighbour_left_behind_does_not_lose_the_seat() {
        let _pinned = pin_dir("inherited");
        let seat = match claim_within(PATIENCE) {
            Claim::Held(s) => s,
            other => panic!("the first claim must be granted, got {other:?}"),
        };
        // What a `Command::spawn` on another thread does to this descriptor
        // between `fork` and `exec`, done deterministically: BSD `flock` counts
        // an inherited descriptor as a second reference to one lock, not a
        // second lock, so the seat is not free the instant this process drops
        // it.
        let inherited = unsafe { libc::dup(held_fd().expect("the seat records its descriptor")) };
        assert!(inherited >= 0, "dup the seat descriptor");
        drop(seat);
        let released = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            unsafe { libc::close(inherited) };
        });
        let again = claim_within(PATIENCE);
        // Joined before the assertion so a failure leaves nothing referencing
        // the seat for the next case to trip over.
        released.join().expect("the reference is let go");
        assert!(
            matches!(again, Claim::Held(_)),
            "the seat comes back once the last reference to it is gone, got {again:?}"
        );
    }

    #[test]
    fn a_lock_left_behind_by_a_dead_holder_is_claimable() {
        let (dir, _guard) = pin_dir("stale");
        let path = dir.join("daemon.lock");
        // The file surviving is exactly what a killed server leaves. It must
        // not read as "taken" — that was the old socket probe's failure mode,
        // in reverse.
        std::fs::write(&path, b"").unwrap();
        assert!(
            matches!(claim_within(PATIENCE), Claim::Held(_)),
            "an unlocked file is not a holder, however it came to exist"
        );
    }
}

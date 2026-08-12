//! Replacing the daemon's binary without replacing the daemon.
//!
//! Every other way of upgrading the daemon ends with the shells dead, and not
//! by oversight. A pty's master is a file descriptor held by this process; when
//! this process goes, the descriptor closes, the slave side raises `SIGHUP`,
//! and everything in the pane goes with it. Storing state does not help — see
//! `daemon::scrollback`, which stores a great deal of it and still cannot bring
//! back a single process.
//!
//! `execve` is the exception. It replaces the *image* while keeping the
//! *process*: same pid, same children, same open descriptors — anything without
//! `FD_CLOEXEC` — same file locks, same signal mask, same session and process
//! group. So the ptys stay open, the shells never see a hangup, and the thing
//! that changes is only which code is on the other end of the descriptor.
//!
//! What that costs is that nothing in memory survives. Threads, the pane
//! registry, the rings, the client connections: all of it is gone the instant
//! `execve` succeeds. Whatever the new image needs, this one has to write down
//! first and pass along by descriptor number.
//!
//! The blob is written to a file that is unlinked before a byte goes into it,
//! so it has no name for anything to read and the kernel frees it when the last
//! descriptor closes. That matters here: it holds every pane's ring, which is
//! the same terminal output `scrollback` makes people opt in to storing. A
//! handoff should not be a way to write it to disk behind their back.
//!
//! Not everything can cross:
//!
//! - **Native SSH panes.** Their session is a cipher state and a set of tasks
//!   in this process's memory. The socket descriptor would survive, but nothing
//!   that knows how to speak on it would. They are hung up before the exec, so
//!   the far end sees a clean disconnect rather than a stalled connection.
//! - **Client connections.** A window is holding a socket to us; that socket
//!   dies with the image. Clients reconnect and reattach by pane id, which is
//!   the path they already use after any restart — except that this time the
//!   attach finds the pane alive, with its ring and its running program.
//! - **Windows.** There is no `execve`, and a ConPTY's pseudoconsole handle
//!   cannot be handed to another process. There the daemon still stops and
//!   starts, and `scrollback` is what softens it.

use std::io::{Read as _, Seek as _, Write as _};
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::daemon::pane::Carried;
use crate::daemon::protocol::WinSize;

/// The flag that tells a starting daemon it is the far side of a handoff. Its
/// value is the descriptor number the blob can be read from.
pub const HANDOFF_FLAG: &str = "--handoff";

/// The descriptor holding this daemon's claim to be *the* daemon.
///
/// Passed rather than re-taken: the lock is still held, by this very process,
/// which is about to become the new one. Asking for it again from a second
/// descriptor would be refused by the kernel, and the new image would stand
/// down in favour of itself.
///
/// It travels on the command line rather than inside the blob because it is the
/// one thing that still matters when the blob cannot be read. A daemon that has
/// lost its panes is a bad afternoon; a daemon that exits because it cannot
/// tell that the lock in its way is its own leaves the machine with no daemon
/// at all.
pub const SEAT_FLAG: &str = "--handoff-seat";

#[derive(Serialize, Deserialize)]
struct Manifest {
    /// Where the new image resumes naming panes. Ids have to keep climbing:
    /// a client that reconnects is holding the old ones, and handing the same
    /// number to a different pane would attach a window to a stranger.
    next_pane_id: u64,
    panes: Vec<PaneRecord>,
}

#[derive(Serialize, Deserialize)]
struct PaneRecord {
    id: u64,
    owner: Option<String>,
    master_fd: RawFd,
    child_pid: u32,
    integration_dir: Option<PathBuf>,
    size: WinSize,
    cwd: Option<PathBuf>,
    #[serde(default)]
    osc_title: Option<String>,
    #[serde(default)]
    shell: Option<crate::daemon::protocol::ShellSpec>,
    shell_active: bool,
    at_prompt: bool,
    last_exit: Option<i32>,
    remote: Option<crate::daemon::protocol::RemoteContext>,
    agent: Option<crate::core::cli_agent::CLIAgent>,
    agent_argv: Option<Vec<String>>,
    agent_session: Option<crate::core::cli_agent::AgentSessionState>,
    /// Length of this pane's ring in the data section, which follows the
    /// manifest in pane order.
    ring_len: u32,
}

/// What this process was told on the command line, if it was started as the far
/// side of a handoff.
#[derive(Debug, PartialEq, Eq)]
pub struct Inheritance {
    pub blob_fd: RawFd,
    pub seat_fd: Option<RawFd>,
}

pub fn requested() -> Option<Inheritance> {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    Some(Inheritance {
        blob_fd: fd_arg(&args, HANDOFF_FLAG)?,
        seat_fd: fd_arg(&args, SEAT_FLAG),
    })
}

fn fd_arg(args: &[std::ffi::OsString], flag: &str) -> Option<RawFd> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg.as_os_str() == std::ffi::OsStr::new(flag) {
            return args.next()?.to_str()?.parse().ok();
        }
        if let Some(value) = arg
            .to_str()
            .and_then(|a| a.strip_prefix(flag).and_then(|r| r.strip_prefix('=')))
        {
            return value.parse().ok();
        }
    }
    None
}

/// Everything the new image is handed.
pub struct Adopted {
    pub panes: Vec<Carried>,
    pub next_pane_id: u64,
}

/// Become the new daemon: write down what the panes are, then `execve`.
///
/// Returns only when the exec did not happen, and in that case nothing has been
/// broken — the descriptors are still open, the panes are still served, and the
/// caller can keep running as if it had never been asked. That ordering is the
/// whole safety argument: the state is staged first and the irreversible step
/// is last, so a failure anywhere before it costs a log line.
pub fn take_over(
    exe: &Path,
    panes: Vec<Carried>,
    next_pane_id: u64,
    seat_fd: Option<RawFd>,
) -> anyhow::Error {
    let blob = match stage(&panes, next_pane_id) {
        Ok(blob) => blob,
        Err(e) => return anyhow::anyhow!("could not stage the handoff: {e}"),
    };

    // Past this point every descriptor the new image needs has to survive the
    // exec, and Rust opens everything close-on-exec. Returning without the exec
    // means putting every flag back: the daemon that keeps serving after a
    // failed handoff spawns children of its own — `lsof`, ssh transports — and
    // a pty master or the seat lock inherited by one of those outlives every
    // assumption this module makes. A child holding the seat keeps the flock
    // held after this daemon dies, and the next daemon would stand down in
    // favour of a process that is not a daemon at all.
    let blob_fd = std::os::fd::AsRawFd::as_raw_fd(&blob);
    let kept: Vec<RawFd> = std::iter::once(blob_fd)
        .chain(seat_fd)
        .chain(panes.iter().map(|p| p.master_fd))
        .collect();
    for (i, fd) in kept.iter().enumerate() {
        if let Err(e) = keep_across_exec(*fd) {
            for fd in &kept[..i] {
                let _ = close_on_exec_again(*fd);
            }
            return anyhow::anyhow!("descriptor {fd} would not survive the exec: {e}");
        }
    }

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--daemon");
    if let Some(dir) = crate::core::config::config_dir_path() {
        cmd.arg("--config-dir").arg(dir);
    }
    cmd.arg(HANDOFF_FLAG).arg(blob_fd.to_string());
    if let Some(seat) = seat_fd {
        cmd.arg(SEAT_FLAG).arg(seat.to_string());
    }

    log::info!(
        "handing {} pane(s) to {} in place; pids and ptys are kept",
        panes.len(),
        exe.display()
    );
    // `exec` resets this thread's signal mask and puts SIGPIPE back to its
    // default before the attempt, and a failed attempt undoes neither — the
    // std docs say as much. A daemon that keeps serving with SIGPIPE fatal
    // dies on the first write to a client that hung up, which is to say: soon.
    let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask) };

    // Only ever returns an error: on success this process is already the new
    // program and this code no longer exists.
    let failure = std::os::unix::process::CommandExt::exec(&mut cmd);
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        libc::pthread_sigmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
    }
    for fd in &kept {
        let _ = close_on_exec_again(*fd);
    }
    anyhow::anyhow!("could not exec {}: {failure}", exe.display())
}

/// Write the blob into a file with no name.
///
/// Created and unlinked before anything is written, so the pane output it
/// carries is reachable only through this descriptor and is freed the moment
/// the last copy of it closes. A crash between here and the exec leaves nothing
/// behind on disk.
fn stage(panes: &[Carried], next_pane_id: u64) -> std::io::Result<std::fs::File> {
    let mut records = Vec::with_capacity(panes.len());
    let mut data = Vec::new();
    for pane in panes {
        let encoded = crate::daemon::scrollback::encode(&pane.ring);
        records.push(PaneRecord {
            id: pane.id,
            owner: pane.owner.clone(),
            master_fd: pane.master_fd,
            child_pid: pane.child_pid,
            integration_dir: pane.integration_dir.clone(),
            size: pane.size,
            cwd: pane.cwd.clone(),
            osc_title: pane.osc_title.clone(),
            shell: pane.shell_spec.clone(),
            shell_active: pane.shell_active,
            at_prompt: pane.at_prompt,
            last_exit: pane.last_exit,
            remote: pane.remote.clone(),
            agent: pane.agent,
            agent_argv: pane.agent_argv.clone(),
            agent_session: pane.agent_session.clone(),
            ring_len: encoded.len() as u32,
        });
        data.extend_from_slice(&encoded);
    }

    let manifest = serde_json::to_vec(&Manifest {
        next_pane_id,
        panes: records,
    })
    .map_err(std::io::Error::other)?;

    let mut file = anonymous_file()?;
    file.write_all(&(manifest.len() as u32).to_le_bytes())?;
    file.write_all(&manifest)?;
    file.write_all(&data)?;
    file.flush()?;
    file.seek(std::io::SeekFrom::Start(0))?;
    Ok(file)
}

fn anonymous_file() -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let dir = std::env::temp_dir();
    for attempt in 0..8 {
        let path = dir.join(format!("tty7-handoff-{}-{attempt}", std::process::id()));
        match std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                // Unlinked immediately: from here the bytes have no name, and
                // the only way to them is the descriptor we are about to pass.
                let _ = std::fs::remove_file(&path);
                return Ok(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(
        "no free name for the handoff file in the temp directory",
    ))
}

fn keep_across_exec(fd: RawFd) -> std::io::Result<()> {
    set_cloexec(fd, false)
}

/// Put close-on-exec back on a descriptor a failed handoff had cleared it from.
pub(crate) fn close_on_exec_again(fd: RawFd) -> std::io::Result<()> {
    set_cloexec(fd, true)
}

fn set_cloexec(fd: RawFd, on: bool) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let flags = if on {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Read back what the previous image left on `fd`.
///
/// A blob that cannot be read is not recoverable and not worth pretending
/// about: the descriptors are still open and the shells are still running, but
/// without the manifest there is no way to know which pane any of them is. The
/// daemon starts empty, the ptys are closed by the kernel when it exits, and
/// the panes read as gone — the same outcome as an ordinary restart.
pub fn adopt(fd: RawFd) -> Option<Adopted> {
    let mut file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    let mut raw = Vec::new();
    if let Err(e) = file.read_to_end(&mut raw) {
        log::error!("could not read the handoff blob on fd {fd}: {e}");
        return None;
    }
    drop(file);

    if raw.len() < 4 {
        log::error!("the handoff blob on fd {fd} is empty");
        return None;
    }
    let manifest_len = u32::from_le_bytes(raw[..4].try_into().ok()?) as usize;
    let manifest: Manifest = match raw.get(4..4 + manifest_len).map(serde_json::from_slice) {
        Some(Ok(manifest)) => manifest,
        Some(Err(e)) => {
            log::error!("the handoff manifest does not parse: {e}");
            return None;
        }
        None => {
            log::error!("the handoff blob is shorter than its own manifest");
            return None;
        }
    };

    let mut cursor = 4 + manifest_len;
    let mut panes = Vec::with_capacity(manifest.panes.len());
    for record in manifest.panes {
        let end = cursor + record.ring_len as usize;
        let ring = raw
            .get(cursor..end)
            .and_then(crate::daemon::scrollback::decode)
            .unwrap_or_default();
        cursor = end;
        panes.push(Carried {
            id: record.id,
            owner: record.owner,
            master_fd: record.master_fd,
            child_pid: record.child_pid,
            integration_dir: record.integration_dir,
            size: record.size,
            ring,
            cwd: record.cwd,
            osc_title: record.osc_title,
            shell_spec: record.shell,
            shell_active: record.shell_active,
            at_prompt: record.at_prompt,
            last_exit: record.last_exit,
            remote: record.remote,
            agent: record.agent,
            agent_argv: record.agent_argv,
            agent_session: record.agent_session,
        });
    }

    Some(Adopted {
        panes,
        next_pane_id: manifest.next_pane_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size() -> WinSize {
        WinSize {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        }
    }

    fn carried(id: u64, fd: RawFd, output: &[u8]) -> Carried {
        Carried {
            id,
            owner: Some("workspace".into()),
            master_fd: fd,
            child_pid: 4242,
            integration_dir: Some(PathBuf::from("/tmp/tty7-int")),
            size: size(),
            ring: vec![crate::daemon::scrollback::Segment {
                size: size(),
                bytes: output.to_vec(),
            }],
            cwd: Some(PathBuf::from("/work")),
            osc_title: Some("user@host:~/work".into()),
            shell_spec: None,
            shell_active: true,
            at_prompt: true,
            last_exit: Some(0),
            remote: None,
            agent: None,
            agent_argv: None,
            agent_session: None,
        }
    }

    #[test]
    fn panes_cross_the_blob_with_their_descriptors_and_their_screens() {
        let staged = stage(
            &[
                carried(7, 31, b"first pane"),
                carried(9, 32, b"second pane"),
            ],
            10,
        )
        .expect("stage the blob");

        let adopted = adopt(std::os::fd::IntoRawFd::into_raw_fd(staged)).expect("read it back");
        assert_eq!(adopted.next_pane_id, 10, "ids must not be handed out twice");
        assert_eq!(adopted.panes.len(), 2);

        assert_eq!(adopted.panes[0].id, 7);
        assert_eq!(
            adopted.panes[0].master_fd, 31,
            "the descriptor number is the pty: without it the new image has a pane id and no pane"
        );
        assert_eq!(adopted.panes[0].child_pid, 4242);
        assert_eq!(adopted.panes[0].ring[0].bytes, b"first pane");
        assert_eq!(adopted.panes[0].cwd, Some(PathBuf::from("/work")));
        assert!(adopted.panes[0].at_prompt);
        assert_eq!(
            adopted.panes[1].ring[0].bytes, b"second pane",
            "each pane's ring has to be read back at its own offset, not the first one's"
        );
    }

    #[test]
    fn a_pane_that_printed_nothing_still_crosses() {
        let mut empty = carried(3, 20, b"");
        empty.ring.clear();
        let staged = stage(&[empty], 4).expect("stage");
        let adopted = adopt(std::os::fd::IntoRawFd::into_raw_fd(staged)).expect("read back");
        assert_eq!(
            adopted.panes.len(),
            1,
            "a silent pane is still a live shell"
        );
        assert!(adopted.panes[0].ring.is_empty());
    }

    #[test]
    fn a_blob_that_is_not_one_is_refused_rather_than_guessed_at() {
        let mut file = anonymous_file().expect("temp file");
        file.write_all(b"nothing like a manifest").expect("write");
        file.seek(std::io::SeekFrom::Start(0)).expect("rewind");
        assert!(
            adopt(std::os::fd::IntoRawFd::into_raw_fd(file)).is_none(),
            "without the manifest there is no way to say which shell is which pane"
        );
    }

    #[test]
    fn the_staged_file_has_no_name_to_read_it_by() {
        let file = anonymous_file().expect("temp file");
        let dir = std::env::temp_dir();
        let leaked: Vec<_> = std::fs::read_dir(&dir)
            .expect("read the temp directory")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("tty7-handoff-"))
            .collect();
        assert!(
            leaked.is_empty(),
            "every pane's output is in this file; it must not be sitting in {} under a name \
             anyone can open",
            dir.display()
        );
        drop(file);
    }

    #[test]
    fn a_failed_exec_puts_close_on_exec_back_on_every_descriptor() {
        use std::os::fd::AsRawFd as _;

        let master = std::fs::File::open("/dev/null").expect("a stand-in master");
        let seat = std::fs::File::open("/dev/null").expect("a stand-in seat");
        let err = take_over(
            Path::new("/nonexistent/tty7-that-cannot-exec"),
            vec![carried(1, master.as_raw_fd(), b"output")],
            2,
            Some(seat.as_raw_fd()),
        );
        assert!(
            err.to_string().contains("could not exec"),
            "the failure was {err}"
        );
        for (name, fd) in [("master", master.as_raw_fd()), ("seat", seat.as_raw_fd())] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(
                flags >= 0 && flags & libc::FD_CLOEXEC != 0,
                "the {name} descriptor is left inheritable: every child this daemon spawns \
                 from now on — a shell, `lsof`, an ssh transport — would hold it open past \
                 this daemon's death"
            );
        }
        // `exec` also put SIGPIPE back to fatal on its way to the attempt. A
        // daemon serving sockets with SIGPIPE fatal dies on the first client
        // that hangs up mid-write.
        let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
        unsafe { libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut act) };
        assert_eq!(
            act.sa_sigaction,
            libc::SIG_IGN,
            "a failed exec must put SIGPIPE back to ignored, the state every Rust process runs in"
        );
    }

    #[test]
    fn the_flags_are_read_in_both_spellings_and_only_when_present() {
        let args = |v: &[&str]| v.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        assert_eq!(
            fd_arg(&args(&["--daemon", "--handoff", "17"]), HANDOFF_FLAG),
            Some(17)
        );
        assert_eq!(
            fd_arg(&args(&["--daemon", "--handoff=17"]), HANDOFF_FLAG),
            Some(17)
        );
        assert_eq!(fd_arg(&args(&["--daemon"]), HANDOFF_FLAG), None);
        assert_eq!(
            fd_arg(&args(&["--handoff", "not-a-number"]), HANDOFF_FLAG),
            None,
            "a daemon that cannot tell which descriptor to read starts clean instead of guessing"
        );
        assert_eq!(
            fd_arg(
                &args(&["--handoff", "17", "--handoff-seat", "18"]),
                SEAT_FLAG
            ),
            Some(18),
            "the seat is read separately, so it is still readable when the blob is not"
        );
    }
}

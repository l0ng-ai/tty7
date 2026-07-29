//! [`RemoteOps`] over a live [`SshConnection`]: command execution on a session
//! channel, file manipulation over SFTP.
//!
//! This is the only file in `install` that talks to a network. Everything it
//! does is a thin, synchronous wrapper — the installer above it is a state
//! machine, and keeping the IO down here as dumb as possible is what lets that
//! state machine be tested against a fake.
//!
//! ## Why the SFTP work is split
//!
//! `stat` / `mkdir` / `chmod` / `rename` / `remove` / `list` all go through
//! [`SftpManager`], which owns one cached SFTP session per connection — the
//! installer costs no extra channel for them. The **byte write** does not:
//! `SftpManager::start_transfer` is the wrong upload path for an install: it is
//! a background job keyed by `pane_id` that reads from a local *file* and
//! reports progress to the GUI's transfer tray, and an install has no pane, no
//! local file (the bytes are in memory, already verified) and nothing to show
//! in a tray. [`SftpManager::put_bytes`] exists for exactly this shape, so the
//! write shares the connection's cached SFTP session — and its
//! retry-once-on-transport-failure behaviour — rather than opening a channel of
//! its own.

use std::sync::Arc;
use std::time::Duration;

use russh::ChannelMsg;

use crate::daemon::protocol::{SftpOp, SftpOpResult};
use crate::daemon::ssh::{SshConnection, SshManager, sftp::SftpManager};

use super::{ExecOutput, RemoteOps, RemoteStat};

/// How long any single remote command may take. Generous: `uname` is instant,
/// but the daemon probe opens a socket on a machine that may be busy, and a
/// distant host's round trips add up. Short enough that a hung sshd surfaces as
/// an error rather than as a connect that never returns.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Budget for the fire-and-forget daemon launch. The remote shell backgrounds
/// the daemon and exits immediately, so this only has to cover a round trip.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

/// [`RemoteOps`] backed by one authenticated SSH connection.
pub struct SshRemoteOps {
    conn: Arc<SshConnection>,
}

impl SshRemoteOps {
    pub fn new(conn: Arc<SshConnection>) -> Self {
        Self { conn }
    }

    /// Run one SFTP op through the shared, cached session.
    fn sftp_op(&self, op: SftpOp) -> Result<SftpOpResult, String> {
        match SftpManager::global().op(&self.conn, &op) {
            SftpOpResult::Error(e) => Err(e),
            other => Ok(other),
        }
    }

    fn block_on<T>(&self, fut: impl Future<Output = T>) -> T {
        SshManager::global().handle().block_on(fut)
    }
}

impl RemoteOps for SshRemoteOps {
    fn home_dir(&self) -> Result<String, String> {
        // SFTP's REALPATH against the session's own working directory, which is
        // the login directory. The same trick the file browser uses to open
        // somewhere better than `/`, and the only way to learn `$HOME` without
        // trusting a shell to have one set.
        match self.sftp_op(SftpOp::Realpath {
            path: ".".to_string(),
        })? {
            SftpOpResult::Link(path) if path.starts_with('/') => Ok(path),
            SftpOpResult::Link(path) => Err(format!(
                "the remote resolved its home to {path:?}, which is not absolute"
            )),
            other => Err(format!(
                "unexpected SFTP reply resolving the home directory: {other:?}"
            )),
        }
    }

    fn run(&self, cmd: &str) -> Result<ExecOutput, String> {
        let conn = self.conn.clone();
        let cmd = cmd.to_string();
        self.block_on(async move {
            match tokio::time::timeout(COMMAND_TIMEOUT, exec(&conn, &cmd)).await {
                Ok(result) => result,
                Err(_) => Err(format!(
                    "the remote did not finish `{cmd}` within {COMMAND_TIMEOUT:?}"
                )),
            }
        })
    }

    fn spawn_detached(&self, cmd: &str) -> Result<(), String> {
        let conn = self.conn.clone();
        let cmd = cmd.to_string();
        // The remote shell backgrounds the process and exits, so this *is* a
        // normal exec — only the budget differs. Its exit status is ignored on
        // purpose: `sh -c '... &'` reports on the backgrounding, never on the
        // daemon, and whether the daemon really came up is settled by probing
        // its socket, not by trusting a shell.
        self.block_on(async move {
            match tokio::time::timeout(LAUNCH_TIMEOUT, exec(&conn, &cmd)).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(format!(
                    "the remote did not accept the daemon launch within {LAUNCH_TIMEOUT:?}"
                )),
            }
        })
    }

    fn stat(&self, path: &str) -> Result<Option<RemoteStat>, String> {
        match self.sftp_op(SftpOp::Stat {
            path: path.to_string(),
        }) {
            Ok(SftpOpResult::Stat(entry)) => Ok(Some(RemoteStat {
                size: entry.size,
                mode: entry.permissions,
                is_dir: entry.kind == crate::daemon::protocol::SftpEntryKind::Dir,
            })),
            Ok(other) => Err(format!("unexpected SFTP reply for stat: {other:?}")),
            // "Not there" is an answer, not a failure — it is the *expected*
            // answer on the first install, and turning it into an error would
            // make step 2 unable to say "go install it".
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        match self.sftp_op(SftpOp::Mkdir {
            path: path.to_string(),
        }) {
            Ok(_) => Ok(()),
            // Servers disagree about which status an existing directory gets
            // (`Failure`, `PermissionDenied`, a bare "file already exists"), so
            // the authority on "does it exist" is a stat, not the error text.
            Err(e) => match self.stat(path) {
                Ok(Some(stat)) if stat.is_dir => Ok(()),
                _ => Err(e),
            },
        }
    }

    fn chmod(&self, path: &str, mode: u32) -> Result<(), String> {
        self.sftp_op(SftpOp::Chmod {
            path: path.to_string(),
            mode,
        })
        .map(|_| ())
    }

    fn put(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        SftpManager::global().put_bytes(&self.conn, path, bytes, &|_| {})
    }

    fn put_with_progress(
        &self,
        path: &str,
        bytes: &[u8],
        on_progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<(), String> {
        SftpManager::global().put_bytes(&self.conn, path, bytes, on_progress)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.sftp_op(SftpOp::Rename {
            from: from.to_string(),
            to: to.to_string(),
        })
        .map(|_| ())
    }

    fn remove_file(&self, path: &str) -> Result<(), String> {
        self.sftp_op(SftpOp::RemoveFile {
            path: path.to_string(),
        })
        .map(|_| ())
    }

    fn list_dir(&self, path: &str) -> Result<Option<Vec<String>>, String> {
        match SftpManager::global().list(&self.conn, path) {
            Ok(entries) => Ok(Some(entries.into_iter().map(|e| e.name).collect())),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Whether a stringified SFTP error means "no such file", which every caller
/// here treats as a normal answer rather than a failure.
///
/// russh-sftp renders a server status as `<code>: <message>`; the message text
/// is the server's, so this matches on the shapes OpenSSH and the common
/// non-OpenSSH servers produce rather than on a code we cannot see.
fn is_not_found(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("no such file")
        || lower.contains("nosuchfile")
        || lower.contains("not found")
        || lower.contains("does not exist")
}

/// Run one command on its own session channel and collect everything it said.
///
/// Loops until `wait()` returns `None` rather than breaking on `Eof`/`Close`:
/// the exit status arrives as its own message and can follow both, and the exit
/// status is the entire point of the daemon probe.
async fn exec(conn: &Arc<SshConnection>, cmd: &str) -> Result<ExecOutput, String> {
    let mut channel = conn
        .open_session_channel()
        .await
        .map_err(|e| format!("could not open a command channel: {e}"))?;
    channel
        .exec(true, cmd)
        .await
        .map_err(|e| format!("could not run `{cmd}`: {e}"))?;
    // Close our end of the command's stdin immediately. Nothing here writes to
    // a command, and the daemon probe specifically relies on its stdin ending
    // so the bridge it starts hangs up instead of parking forever.
    let _ = channel.eof().await;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
            ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
            _ => {}
        }
    }

    Ok(ExecOutput {
        status,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The "absent" classification has to hold for the wordings the servers we
    /// meet actually use, because step 2's whole decision ("is the right version
    /// already installed?") rests on it — and misreading a missing file as an
    /// error would turn every first install into a hard failure.
    #[test]
    fn missing_files_are_recognised_across_server_wordings() {
        for msg in [
            "2: No such file",
            "No such file or directory",
            "NoSuchFile",
            "file not found",
            "The system cannot find the path specified: does not exist",
        ] {
            assert!(is_not_found(msg), "{msg:?} means absent");
        }
    }

    /// And must not swallow the failures that have to be reported: a full
    /// disk or a read-only home has to surface as an error with a path, never as
    /// "the file isn't there, go ahead and install".
    #[test]
    fn real_failures_are_not_mistaken_for_absence() {
        for msg in [
            "3: Permission denied",
            "4: Failure",
            "no space left on device",
            "disk quota exceeded",
            "connection reset by peer",
        ] {
            assert!(!is_not_found(msg), "{msg:?} is a failure, not an absence");
        }
    }
}

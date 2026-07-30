use std::sync::Arc;
use std::time::Duration;

use russh::ChannelMsg;

use crate::daemon::protocol::{SftpOp, SftpOpResult};
use crate::daemon::ssh::{SshConnection, SshManager, sftp::SftpManager};

use super::{ExecOutput, RemoteOps, RemoteStat};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

pub struct SshRemoteOps {
    conn: Arc<SshConnection>,
}

impl SshRemoteOps {
    pub fn new(conn: Arc<SshConnection>) -> Self {
        Self { conn }
    }

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
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn mkdir(&self, path: &str) -> Result<(), String> {
        match self.sftp_op(SftpOp::Mkdir {
            path: path.to_string(),
        }) {
            Ok(_) => Ok(()),
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

fn is_not_found(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("no such file")
        || lower.contains("nosuchfile")
        || lower.contains("not found")
        || lower.contains("does not exist")
}

async fn exec(conn: &Arc<SshConnection>, cmd: &str) -> Result<ExecOutput, String> {
    let mut channel = conn
        .open_session_channel()
        .await
        .map_err(|e| format!("could not open a command channel: {e}"))?;
    channel
        .exec(true, cmd)
        .await
        .map_err(|e| format!("could not run `{cmd}`: {e}"))?;
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

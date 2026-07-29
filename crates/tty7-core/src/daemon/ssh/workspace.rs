//! Workspace-scoped control requests (M7).
//!
//! A *remote workspace* has no pane on this daemon: its
//! panes live on the remote `tty7-server` and reach it through a routed byte
//! pipe. What this side owns is the [`SshConnection`] that pipe rides — the same
//! connection an SSH pane to that host would have used, deduplicated by
//! [`ConnectionKey`].
//!
//! Everything a user wants from that connection — a port forward behind a
//! ⌘-clicked `localhost:3000`, an SFTP download dragged out to Finder — is
//! therefore addressable, just not by `pane_id`. This module is the one place
//! that translates "which workspace" into "which connection", and it is
//! deliberately the *only* new entry point: [`handle`] answers a whole
//! [`WorkspaceRequest`] with a ready-to-send [`DaemonMsg`], so the daemon's
//! dispatch grows one arm rather than nine.
//!
//! **It never connects.** [`SshManager::existing_connection`] is a lookup: a
//! workspace request arrives on a short-lived control connection with nowhere to
//! put an auth prompt, so "no connection" is reported as an error the GUI can
//! show rather than a silent connect attempt that would hang on a passphrase.

use crate::core::session::WorkspaceId;
use crate::daemon::protocol::{DaemonMsg, WorkspaceOp, WorkspaceRequest};

use super::SshManager;
use super::sftp::SftpManager;

/// The bucket a workspace's SFTP transfer jobs are filed under.
///
/// [`SftpManager`] keys jobs by `pane_id`, and a remote workspace has no pane
/// here to lend it one. Deriving the key from the workspace instead of using the
/// requesting pane keeps a running download visible after the user switches the
/// Files panel to another pane — the transfer belongs to the machine, not to
/// whichever tab happened to start it.
///
/// The top bit is set so the synthetic key cannot collide with a real pane id:
/// pane ids come from a counter that starts at 1, so a collision would need 2^63
/// panes in one daemon.
pub fn job_key(workspace: WorkspaceId) -> u64 {
    workspace.element_key() | (1 << 63)
}

/// Answer one [`WorkspaceRequest`].
///
/// Every failure — an unknown/disconnected workspace, a refused bind, an SFTP
/// error — comes back as [`DaemonMsg::Error`] with a sentence the GUI can show
/// verbatim, because the caller has no other channel to explain itself on.
pub fn handle(req: &WorkspaceRequest) -> DaemonMsg {
    let mgr = SshManager::global();
    let Some(conn) = mgr.existing_connection(&req.spec) else {
        // The workspace is not connected (or is mid-reconnect). Naming the host
        // matters: with several windows open the user needs to know *which* one
        // went away.
        return DaemonMsg::Error(format!(
            "workspace is not connected to {}@{}:{} — reconnect the window and try again",
            req.spec.user, req.spec.host, req.spec.port
        ));
    };
    let ws = req.workspace;
    let view = req.view_pane;

    match &req.op {
        WorkspaceOp::EnsureLoopback {
            remote_host,
            remote_port,
        } => match mgr.ensure_workspace_loopback(ws, conn, remote_host, *remote_port) {
            Ok(forward) => DaemonMsg::LoopbackForward(forward),
            Err(e) => DaemonMsg::Error(format!("forward failed: {e}")),
        },
        WorkspaceOp::AddForward { rule } => {
            DaemonMsg::ForwardList(mgr.add_workspace_forward(ws, view, conn, rule))
        }
        WorkspaceOp::RemoveForward { forward_id } => {
            DaemonMsg::ForwardList(mgr.remove_workspace_forward(ws, view, *forward_id))
        }
        WorkspaceOp::ListForwards => DaemonMsg::ForwardList(mgr.list_workspace_forwards(ws, view)),
        WorkspaceOp::TeardownForwards => {
            mgr.teardown_workspace_forwards(ws);
            DaemonMsg::ForwardList(mgr.list_workspace_forwards(ws, view))
        }
        WorkspaceOp::SftpList { path } => match SftpManager::global().list(&conn, path) {
            Ok(entries) => DaemonMsg::SftpEntries(entries),
            Err(e) => DaemonMsg::Error(e),
        },
        WorkspaceOp::SftpOp { op } => DaemonMsg::SftpOpResult(SftpManager::global().op(&conn, op)),
        WorkspaceOp::SftpTransferStart { spec } => {
            // The caller's `pane_id` is overridden rather than trusted: a
            // workspace's jobs must land in the workspace's bucket, or
            // `SftpTransferList` below would not find them again.
            let mut spec = spec.clone();
            spec.pane_id = job_key(ws);
            match SftpManager::global().start_transfer(&conn, spec) {
                Ok(job_id) => DaemonMsg::SftpTransferStarted { job_id },
                Err(e) => DaemonMsg::Error(e),
            }
        }
        WorkspaceOp::SftpTransferList => {
            DaemonMsg::SftpTransferProgress(SftpManager::global().list_jobs(job_key(ws)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The synthetic job bucket is stable for a workspace, distinct between
    /// workspaces, and out of reach of any real pane id.
    #[test]
    fn job_key_is_stable_distinct_and_out_of_pane_range() {
        let a = WorkspaceId::new();
        let b = WorkspaceId::new();
        assert_eq!(job_key(a), job_key(a), "stable across calls");
        assert_ne!(job_key(a), job_key(b));
        // Real pane ids come from a counter starting at 1; none of them has the
        // top bit set, so the two spaces cannot overlap.
        assert!(job_key(a) >= 1 << 63);
        assert!(job_key(b) >= 1 << 63);
    }

    /// A request naming a host this daemon has no connection to is refused with a
    /// message that names the host — never by silently connecting (which would
    /// need credentials this path cannot prompt for).
    #[test]
    fn request_without_a_live_connection_is_refused_by_name() {
        // Built through serde so the test states only the three fields it cares
        // about; every other field of `NativeSshSpec` has a serde default.
        let spec: crate::daemon::protocol::NativeSshSpec = serde_json::from_str(
            r#"{"host":"nowhere.invalid","port":2222,"user":"someone","auth_mode":"auto"}"#,
        )
        .unwrap();
        let req = WorkspaceRequest {
            workspace: WorkspaceId::new(),
            spec: Box::new(spec),
            view_pane: 3,
            op: WorkspaceOp::ListForwards,
        };
        match handle(&req) {
            DaemonMsg::Error(e) => {
                assert!(e.contains("someone@nowhere.invalid:2222"), "got: {e}");
                assert!(e.contains("not connected"), "got: {e}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}

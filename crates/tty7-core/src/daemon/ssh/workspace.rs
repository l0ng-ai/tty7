use crate::core::session::WorkspaceId;
use crate::daemon::protocol::{DaemonMsg, WorkspaceOp, WorkspaceRequest};

use super::SshManager;
use super::sftp::SftpManager;

pub fn job_key(workspace: WorkspaceId) -> u64 {
    workspace.element_key() | (1 << 63)
}

pub fn handle(req: &WorkspaceRequest) -> DaemonMsg {
    let mgr = SshManager::global();
    let Some(conn) = mgr.existing_connection(&req.spec) else {
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

    #[test]
    fn job_key_is_stable_distinct_and_out_of_pane_range() {
        let a = WorkspaceId::new();
        let b = WorkspaceId::new();
        assert_eq!(job_key(a), job_key(a), "stable across calls");
        assert_ne!(job_key(a), job_key(b));
        assert!(job_key(a) >= 1 << 63);
        assert!(job_key(b) >= 1 << 63);
    }

    #[test]
    fn request_without_a_live_connection_is_refused_by_name() {
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

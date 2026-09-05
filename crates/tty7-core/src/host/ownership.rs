//! Resume rights outlive a socket, but never outlive this server instance.
//! A takeover rotates the opaque proof, including when the new holder goes
//! offline before the displaced client reconnects.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, Weak};

use crate::core::session::WorkspaceId;
use crate::daemon::control::LinkShutdown;
use crate::daemon::control::WorkspaceProof;

/// Commands hold a read permit only while being applied, never while waiting
/// for network input. Revocation is nonblocking: a stalled PTY writer cannot
/// freeze the host-wide handover lock, and no new owner is granted until the
/// previous operation has finished.
pub(crate) struct PaneLease {
    token: WorkspaceProof,
    active: RwLock<bool>,
    invalidated: AtomicBool,
    streams: Mutex<Vec<Weak<dyn LinkShutdown>>>,
}

impl PaneLease {
    fn new() -> Self {
        Self {
            token: WorkspaceProof::fresh(),
            active: RwLock::new(true),
            invalidated: AtomicBool::new(false),
            streams: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn enter(&self) -> io::Result<RwLockReadGuard<'_, bool>> {
        let permit = self.active.read().unwrap_or_else(|e| e.into_inner());
        if !*permit || self.invalidated.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workspace pane capability has been revoked; resume the workspace",
            ));
        }
        Ok(permit)
    }

    pub(crate) fn register(&self, closer: &Arc<dyn LinkShutdown>) -> io::Result<()> {
        let _permit = self.enter()?;
        let mut streams = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        if self.invalidated.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workspace pane capability has been revoked",
            ));
        }
        streams.retain(|stream| stream.strong_count() > 0);
        streams.push(Arc::downgrade(closer));
        Ok(())
    }

    fn revoke(&self) -> io::Result<()> {
        self.revoke_after(|| Ok(()))
    }

    fn revoke_after<T>(&self, action: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let mut active = self.active.try_write().map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "a pane command is still in flight; retry workspace takeover after it finishes",
            )
        })?;
        let result = action()?;
        *active = false;
        self.invalidate();
        Ok(result)
    }

    fn invalidate(&self) {
        self.invalidated.store(true, Ordering::Release);
        for stream in self
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            if let Some(stream) = stream.upgrade() {
                let _ = stream.shutdown_link();
            }
        }
    }
}

#[derive(Default)]
pub(super) struct Ownership {
    grants: HashMap<WorkspaceId, Grant>,
}

struct Grant {
    proof: WorkspaceProof,
    hostname: String,
    pane: Arc<PaneLease>,
}

impl Ownership {
    pub(super) fn can_resume(
        &self,
        workspace: WorkspaceId,
        proof: Option<&WorkspaceProof>,
    ) -> Result<(), String> {
        match self.grants.get(&workspace) {
            Some(grant) if Some(&grant.proof) != proof => Err(grant.hostname.clone()),
            _ => Ok(()),
        }
    }

    pub(super) fn revoke_panes(&self, workspace: WorkspaceId) -> io::Result<()> {
        if let Some(grant) = self.grants.get(&workspace) {
            grant.pane.revoke()?;
        }
        Ok(())
    }

    pub(super) fn invalidate_panes(&self, workspace: WorkspaceId) {
        if let Some(grant) = self.grants.get(&workspace) {
            grant.pane.invalidate();
        }
    }

    pub(super) fn retire_panes_after<T>(
        &self,
        workspace: WorkspaceId,
        action: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        match self.grants.get(&workspace) {
            Some(grant) => grant.pane.revoke_after(action),
            None => action(),
        }
    }

    pub(super) fn renew_panes(&mut self, workspace: WorkspaceId) -> WorkspaceProof {
        let grant = self
            .grants
            .get_mut(&workspace)
            .expect("workspace acquired first");
        grant.pane = Arc::new(PaneLease::new());
        grant.pane.token.clone()
    }

    pub(super) fn pane_lease(
        &self,
        auth: &crate::daemon::protocol::PaneAuthorization,
    ) -> io::Result<Arc<PaneLease>> {
        self.grants
            .get(&auth.workspace)
            .filter(|grant| grant.pane.token == auth.token)
            .map(|grant| grant.pane.clone())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "invalid or expired workspace pane capability",
                )
            })
    }
    pub(super) fn contains(&self, workspace: WorkspaceId) -> bool {
        self.grants.contains_key(&workspace)
    }

    pub(super) fn resume(
        &mut self,
        workspace: WorkspaceId,
        proof: Option<&WorkspaceProof>,
        hostname: &str,
    ) -> Result<WorkspaceProof, String> {
        match self.grants.get(&workspace) {
            Some(grant) if Some(&grant.proof) == proof => Ok(grant.proof.clone()),
            Some(grant) => Err(grant.hostname.clone()),
            None => Ok(self.take_over(workspace, hostname)),
        }
    }

    pub(super) fn take_over(&mut self, workspace: WorkspaceId, hostname: &str) -> WorkspaceProof {
        let proof = WorkspaceProof::fresh();
        self.grants.insert(
            workspace,
            Grant {
                proof: proof.clone(),
                hostname: hostname.into(),
                pane: Arc::new(PaneLease::new()),
            },
        );
        proof
    }

    pub(super) fn forget(&mut self, workspace: WorkspaceId) {
        self.invalidate_panes(workspace);
        self.grants.remove(&workspace);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revocation_refuses_inflight_work_without_waiting_or_partially_changing_the_lease() {
        let lease = PaneLease::new();
        let other = PaneLease::new();
        let operation = lease.enter().unwrap();
        assert_eq!(
            lease.revoke().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(!lease.invalidated.load(Ordering::Acquire));
        assert!(
            other.enter().is_ok(),
            "an unrelated workspace remains usable"
        );
        drop(operation);
        lease.revoke().unwrap();
        assert_eq!(
            lease.enter().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn invalidation_closes_registered_streams_and_rejects_late_registration() {
        struct Closer(AtomicBool);
        impl LinkShutdown for Closer {
            fn shutdown_link(&self) -> io::Result<()> {
                self.0.store(true, Ordering::Release);
                Ok(())
            }
        }
        let lease = PaneLease::new();
        let closer = Arc::new(Closer(AtomicBool::new(false)));
        let erased: Arc<dyn LinkShutdown> = closer.clone();
        lease.register(&erased).unwrap();
        let admitted = lease.enter().unwrap();
        lease.invalidate();
        assert!(closer.0.load(Ordering::Acquire));
        assert!(lease.enter().is_err());
        assert!(lease.register(&erased).is_err());
        assert!(lease.streams.lock().unwrap().is_empty());
        drop(admitted);
        lease.revoke().unwrap();
    }

    #[test]
    fn only_the_issued_proof_can_resume_even_after_a_socket_disappears() {
        let mut ownership = Ownership::default();
        let workspace = WorkspaceId::new();
        let proof = ownership.resume(workspace, None, "laptop").unwrap();
        assert_eq!(
            ownership.resume(workspace, None, "laptop"),
            Err("laptop".into())
        );
        assert_eq!(
            ownership.resume(workspace, Some(&WorkspaceProof::fresh()), "desktop"),
            Err("laptop".into())
        );
        assert_eq!(
            ownership.resume(workspace, Some(&proof), "laptop"),
            Ok(proof)
        );
    }

    #[test]
    fn takeover_revokes_the_old_proof_permanently_for_this_instance() {
        let mut ownership = Ownership::default();
        let workspace = WorkspaceId::new();
        let old = ownership.resume(workspace, None, "laptop").unwrap();
        let current = ownership.take_over(workspace, "desktop");
        assert_ne!(old, current);
        assert_eq!(
            ownership.resume(workspace, Some(&old), "laptop"),
            Err("desktop".into())
        );
        assert_eq!(
            ownership.resume(workspace, Some(&current), "desktop"),
            Ok(current)
        );
    }

    #[test]
    fn proof_for_one_workspace_does_not_authorize_another() {
        let mut ownership = Ownership::default();
        let a = WorkspaceId::new();
        let b = WorkspaceId::new();
        let proof = ownership.take_over(a, "a");
        ownership.take_over(b, "b");
        assert_eq!(ownership.resume(b, Some(&proof), "a"), Err("b".into()));
        ownership.forget(b);
        assert!(ownership.resume(b, None, "new").is_ok());
    }
}

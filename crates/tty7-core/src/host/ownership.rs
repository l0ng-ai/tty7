//! Resume rights outlive a socket, but never outlive this server instance.
//! A takeover rotates the opaque proof, including when the new holder goes
//! offline before the displaced client reconnects.

use std::collections::HashMap;

use crate::core::session::WorkspaceId;
use crate::daemon::control::WorkspaceProof;

#[derive(Default)]
pub(super) struct Ownership {
    grants: HashMap<WorkspaceId, Grant>,
}

struct Grant {
    proof: WorkspaceProof,
    hostname: String,
}

impl Ownership {
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
            },
        );
        proof
    }

    pub(super) fn forget(&mut self, workspace: WorkspaceId) {
        self.grants.remove(&workspace);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

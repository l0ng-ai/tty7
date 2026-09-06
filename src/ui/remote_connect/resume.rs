use std::io::Read as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gpui::{App, Global};
use serde::{Deserialize, Serialize};
use tty7_core::daemon::control::WorkspaceProof;

#[derive(Clone, Default)]
pub struct ResumeProofs(Arc<Mutex<Cache>>);
impl Global for ResumeProofs {}

#[derive(Default)]
struct Cache {
    path: Option<PathBuf>,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    target: String,
    workspace: String,
    instance: String,
    proof: WorkspaceProof,
}

impl ResumeProofs {
    pub fn install(cx: &mut App) {
        let path = crate::core::config::config_path("remote-resume.json");
        cx.set_global(Self::load(path));
    }

    fn load(path: Option<PathBuf>) -> Self {
        let entries = path
            .as_ref()
            .and_then(|path| {
                let mut bytes = Vec::new();
                std::fs::File::open(path)
                    .ok()?
                    .take(4 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .ok()?;
                if bytes.len() > 4 * 1024 * 1024 {
                    return None;
                }
                serde_json::from_slice::<Vec<Entry>>(&bytes).ok()
            })
            .unwrap_or_default();
        Self(Arc::new(Mutex::new(Cache { path, entries })))
    }

    pub fn get(&self, target: &str, workspace: &str, instance: &str) -> Option<WorkspaceProof> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .iter()
            .find(|e| e.target == target && e.workspace == workspace && e.instance == instance)
            .map(|e| e.proof.clone())
    }

    pub fn remember(
        &self,
        target: String,
        workspace: String,
        instance: String,
        proof: WorkspaceProof,
        cx: &App,
    ) {
        {
            let mut cache = self.0.lock().unwrap_or_else(|e| e.into_inner());
            cache
                .entries
                .retain(|e| e.target != target || e.workspace != workspace);
            cache.entries.push(Entry {
                target,
                workspace,
                instance,
                proof,
            });
            if cache.entries.len() > 4096 {
                cache.entries.remove(0);
            }
        }
        let cache = self.0.clone();
        cx.background_executor()
            .spawn(async move {
                // Serialize the latest state while holding the writer lock: two
                // asynchronous saves cannot publish an older proof over a new one.
                let cache = cache.lock().unwrap_or_else(|e| e.into_inner());
                let result = cache.save();
                if let Err(error) = result {
                    log::warn!("could not persist remote resume credentials: {error}");
                }
            })
            .detach();
    }
}

impl Cache {
    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec(&self.entries)?;
        crate::core::config::write_atomic_private(path, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_proofs_are_bound_to_target_workspace_and_server_instance() {
        let proof: WorkspaceProof = serde_json::from_str("\"test-proof\"").unwrap();
        let cache = ResumeProofs(Arc::new(Mutex::new(Cache {
            path: None,
            entries: vec![Entry {
                target: "target".into(),
                workspace: "workspace".into(),
                instance: "instance".into(),
                proof: proof.clone(),
            }],
        })));
        assert_eq!(cache.get("target", "workspace", "instance"), Some(proof));
        assert!(cache.get("other", "workspace", "instance").is_none());
        assert!(cache.get("target", "other", "instance").is_none());
        assert!(cache.get("target", "workspace", "restarted").is_none());
    }

    #[test]
    fn a_saved_proof_survives_restarting_the_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote-resume.json");
        let proof: WorkspaceProof = serde_json::from_str("\"private-test-proof\"").unwrap();
        Cache {
            path: Some(path.clone()),
            entries: vec![Entry {
                target: "target".into(),
                workspace: "workspace".into(),
                instance: "instance".into(),
                proof: proof.clone(),
            }],
        }
        .save()
        .unwrap();
        let reloaded = ResumeProofs::load(Some(path));
        assert_eq!(reloaded.get("target", "workspace", "instance"), Some(proof));
    }
}

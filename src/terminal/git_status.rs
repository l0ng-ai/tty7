use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use crate::core::git::{GitStatus, RepoSnapshot, branch_name, git, probe};
use crate::ui::host_ops::{ByHost, HostId, InFlight};

#[derive(Default)]
pub struct GitStatusCache {
    roots: ByHost<PathBuf, Option<PathBuf>>,
    homes: ByHost<PathBuf, PathBuf>,
    status: ByHost<PathBuf, GitStatus>,
    probes: InFlight<(HostId, PathBuf)>,
    last_probe: ByHost<PathBuf, Instant>,
}

impl gpui::Global for GitStatusCache {}

impl GitStatusCache {
    pub fn status_for(&self, host: HostId, cwd: &Path) -> Option<GitStatus> {
        let root = self.roots.get(host, cwd)?.as_ref()?;
        self.status.get(host, root).cloned()
    }

    pub fn known_repo_for(&self, host: HostId, cwd: &Path) -> Option<Option<PathBuf>> {
        let root = self.roots.get(host, cwd)?;
        Some(root.as_ref().map(|root| {
            self.homes
                .get(host, root)
                .cloned()
                .unwrap_or_else(|| root.clone())
        }))
    }

    pub fn begin_probe(&mut self, host: HostId, cwd: &Path) -> bool {
        let key = (host, cwd.to_path_buf());
        if self.probes.begin(key.clone()) {
            true
        } else {
            self.probes.invalidate(&key);
            false
        }
    }

    pub fn begin_probe_throttled(
        &mut self,
        host: HostId,
        cwd: &Path,
        min_interval: Duration,
    ) -> bool {
        if self.probes.is_pending(&(host, cwd.to_path_buf())) {
            return false;
        }
        let key = self.throttle_key(host, cwd).to_path_buf();
        if self
            .last_probe
            .get(host, key.as_path())
            .is_some_and(|at| at.elapsed() < min_interval)
        {
            return false;
        }
        self.last_probe.insert(host, key, Instant::now());
        self.probes.begin((host, cwd.to_path_buf()));
        true
    }

    fn throttle_key<'a>(&'a self, host: HostId, cwd: &'a Path) -> &'a Path {
        match self.roots.get(host, cwd) {
            Some(Some(root)) => root,
            _ => cwd,
        }
    }

    pub fn finish_probe(
        &mut self,
        host: HostId,
        cwd: &Path,
        snapshot: Option<RepoSnapshot>,
    ) -> bool {
        let rerun = !self.probes.finish(&(host, cwd.to_path_buf()));
        let key = match &snapshot {
            Some(snap) => snap.root.clone(),
            None => self.throttle_key(host, cwd).to_path_buf(),
        };
        self.last_probe.insert(host, key, Instant::now());
        match snapshot {
            Some(snap) => {
                let (added, removed) = snap.counts.unwrap_or_else(|| {
                    self.status
                        .get(host, &snap.root)
                        .map(|g| (g.added, g.removed))
                        .unwrap_or((0, 0))
                });
                self.status.insert(
                    host,
                    snap.root.clone(),
                    GitStatus {
                        branch: snap.branch,
                        added,
                        removed,
                    },
                );
                self.homes.insert(host, snap.root.clone(), snap.home);
                self.roots.insert(host, cwd.to_path_buf(), Some(snap.root));
            }
            None => {
                self.roots.insert(host, cwd.to_path_buf(), None);
            }
        }
        rerun
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: HostId = HostId::LOCAL;

    fn snap(root: &str, branch: &str, counts: Option<(u32, u32)>) -> RepoSnapshot {
        RepoSnapshot {
            root: PathBuf::from(root),
            home: PathBuf::from(root),
            branch: branch.into(),
            counts,
        }
    }

    fn wt_snap(root: &str, home: &str, branch: &str) -> RepoSnapshot {
        RepoSnapshot {
            root: PathBuf::from(root),
            home: PathBuf::from(home),
            branch: branch.into(),
            counts: Some((0, 0)),
        }
    }
    #[test]
    fn cwds_in_one_repo_share_a_snapshot() {
        let mut cache = GitStatusCache::default();
        let (a, b) = (Path::new("/repo/sub/a"), Path::new("/repo"));
        cache.finish_probe(L, a, Some(snap("/repo", "main", Some((5, 2)))));
        cache.finish_probe(L, b, Some(snap("/repo", "main", Some((5, 2)))));
        cache.finish_probe(L, a, Some(snap("/repo", "main", Some((200, 42)))));
        for cwd in [a, b] {
            let got = cache.status_for(L, cwd).unwrap();
            assert_eq!((got.added, got.removed), (200, 42), "cwd {cwd:?}");
        }
    }

    #[test]
    fn one_path_on_two_machines_is_two_entries() {
        let mut cache = GitStatusCache::default();
        let remote = HostId::from_connection_key("ssh-direct:me@box:22");
        let cwd = Path::new("/src/app");

        cache.finish_probe(L, cwd, Some(snap("/src/app", "main", Some((1, 2)))));
        cache.finish_probe(
            remote,
            cwd,
            Some(snap("/src/app", "feat/x", Some((30, 40)))),
        );

        let local = cache.status_for(L, cwd).unwrap();
        let there = cache.status_for(remote, cwd).unwrap();
        assert_eq!((local.branch.as_str(), local.added), ("main", 1));
        assert_eq!((there.branch.as_str(), there.added), ("feat/x", 30));

        cache.finish_probe(
            remote,
            cwd,
            Some(wt_snap("/src/app", "/src/main", "feat/x")),
        );
        assert_eq!(
            cache.known_repo_for(L, cwd),
            Some(Some(PathBuf::from("/src/app")))
        );
        assert_eq!(
            cache.known_repo_for(remote, cwd),
            Some(Some(PathBuf::from("/src/main")))
        );

        assert!(cache.begin_probe(L, cwd));
        assert!(cache.begin_probe(remote, cwd));
        assert!(!cache.begin_probe(L, cwd), "same host, already flying");
        assert!(cache.finish_probe(L, cwd, None), "…so it asks for a rerun");
        assert!(!cache.finish_probe(remote, cwd, None), "the other did not");

        let gap = Duration::from_secs(60);
        assert!(!cache.begin_probe_throttled(L, cwd, gap), "just landed");
        assert!(
            !cache.begin_probe_throttled(remote, cwd, gap),
            "just landed"
        );
        let other = Path::new("/elsewhere");
        assert!(cache.begin_probe_throttled(L, other, gap));
        assert!(cache.begin_probe_throttled(remote, other, gap));
    }

    #[test]
    fn failed_diff_keeps_previous_counts() {
        let mut cache = GitStatusCache::default();
        let cwd = Path::new("/repo");
        cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((200, 42)))));
        cache.finish_probe(L, cwd, Some(snap("/repo", "feat/x", None)));
        let got = cache.status_for(L, cwd).unwrap();
        assert_eq!(got.branch, "feat/x");
        assert_eq!((got.added, got.removed), (200, 42));
    }

    #[test]
    fn concurrent_triggers_fold_into_one_probe_then_rerun() {
        let mut cache = GitStatusCache::default();
        let cwd = Path::new("/repo");
        assert!(cache.begin_probe(L, cwd));
        assert!(!cache.begin_probe(L, cwd));
        assert!(cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((1, 0))))));
        assert!(cache.begin_probe(L, cwd));
        assert!(!cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((1, 0))))));
    }

    #[test]
    fn non_repo_cwd_clears_only_itself() {
        let mut cache = GitStatusCache::default();
        let (a, b) = (Path::new("/repo/a"), Path::new("/repo/b"));
        cache.finish_probe(L, a, Some(snap("/repo", "main", Some((3, 1)))));
        cache.finish_probe(L, b, Some(snap("/repo", "main", Some((3, 1)))));
        cache.finish_probe(L, a, None);
        assert_eq!(cache.status_for(L, a), None);
        assert!(cache.status_for(L, b).is_some());
    }

    #[test]
    fn known_repo_for_is_three_valued() {
        let mut cache = GitStatusCache::default();
        let (repo, plain, unseen) = (
            Path::new("/repo/a"),
            Path::new("/tmp/x"),
            Path::new("/never"),
        );
        cache.finish_probe(L, repo, Some(snap("/repo", "main", Some((1, 0)))));
        cache.finish_probe(L, plain, None);

        assert_eq!(
            cache.known_repo_for(L, repo),
            Some(Some(PathBuf::from("/repo")))
        );
        assert_eq!(cache.known_repo_for(L, plain), Some(None));
        assert_eq!(cache.known_repo_for(L, unseen), None);
    }

    #[test]
    fn worktrees_share_a_repo_but_not_a_status() {
        let mut cache = GitStatusCache::default();
        let (main, wt) = (Path::new("/repo"), Path::new("/repo/.wt/feat"));
        cache.finish_probe(L, main, Some(wt_snap("/repo", "/repo", "main")));
        cache.finish_probe(L, wt, Some(wt_snap("/repo/.wt/feat", "/repo", "feat/x")));

        assert_eq!(
            cache.known_repo_for(L, main),
            Some(Some(PathBuf::from("/repo")))
        );
        assert_eq!(
            cache.known_repo_for(L, wt),
            Some(Some(PathBuf::from("/repo")))
        );
        assert_eq!(cache.status_for(L, main).unwrap().branch, "main");
        assert_eq!(cache.status_for(L, wt).unwrap().branch, "feat/x");
    }
    #[test]
    fn throttled_probes_decline_instead_of_queueing() {
        let mut cache = GitStatusCache::default();
        let cwd = Path::new("/repo");
        let gap = Duration::from_secs(60);

        assert!(cache.begin_probe_throttled(L, cwd, gap));
        assert!(!cache.begin_probe_throttled(L, cwd, gap));
        assert!(!cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((1, 0))))));

        assert!(!cache.begin_probe_throttled(L, cwd, gap));
        assert!(cache.begin_probe_throttled(L, cwd, Duration::ZERO));
        assert!(!cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((1, 0))))));
        assert!(cache.begin_probe(L, cwd));
    }

    #[test]
    fn throttle_collapses_subdirectories_of_one_repo() {
        let mut cache = GitStatusCache::default();
        let (top, src, docs) = (
            Path::new("/repo"),
            Path::new("/repo/src"),
            Path::new("/repo/docs"),
        );
        let gap = Duration::from_secs(60);

        for cwd in [top, src, docs] {
            assert!(cache.begin_probe_throttled(L, cwd, gap));
            assert!(!cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((3, 1))))));
        }

        assert!(!cache.begin_probe_throttled(L, top, gap));
        assert!(!cache.begin_probe_throttled(L, src, gap));

        assert!(cache.begin_probe_throttled(L, docs, Duration::ZERO));
        assert!(!cache.begin_probe_throttled(L, top, gap));
        assert!(!cache.begin_probe_throttled(L, src, gap));

        let other = Path::new("/other");
        assert!(cache.begin_probe_throttled(L, other, gap));
    }
}

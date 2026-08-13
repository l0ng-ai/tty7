use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use crate::core::git::{GitStatus, RepoSnapshot, probe};
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

    /// The working tree `cwd` is in, if this cache has already found out.
    ///
    /// Distinct from [`GitStatusCache::known_repo_for`], which answers with the
    /// *home* — the main working tree a linked one belongs to, which is what
    /// a "which project is this" question wants. This answers with the root,
    /// which is the key everything git-shaped is stored under.
    pub fn repo_root_for(&self, host: HostId, cwd: &Path) -> Option<&Path> {
        self.roots.get(host, cwd)?.as_deref()
    }

    /// Forget a machine we have stopped talking to, so a reconnect starts from
    /// nothing rather than from whatever it looked like on the way down.
    pub fn clear_host(&mut self, host: HostId) {
        self.roots.clear_host(host);
        self.homes.clear_host(host);
        self.status.clear_host(host);
        self.last_probe.clear_host(host);
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

    /// Corrects a repo's branch and line counts from a diff that was just read.
    ///
    /// The status here is refreshed on an edge — a command finishing, the
    /// directory changing, the window coming back — and a diff read by the
    /// Changes panel or the diff overlay is a fresher answer to the same
    /// question. Without this the sidebar can say +27 −8 while the overlay it
    /// opens says +3 −1. The root and the probe's own schedule are left alone.
    ///
    /// The branch moves with the numbers, and has to. A diff snapshot names the
    /// branch from the same `git branch_name` call the status probe uses, so
    /// the two are the same answer read at different moments — but the overlay
    /// treats a disagreement between them as "the repository changed under me"
    /// and re-probes. Leaving the branch behind made that disagreement
    /// permanent whenever anything switched branches outside tty7: the overlay
    /// re-read the diff, published it, woke every watcher of this cache, found
    /// the same disagreement, and went round again — two `git` processes per
    /// lap, forever, with `refreshing…` pinned to the header.
    pub fn note_diff_read(
        &mut self,
        host: HostId,
        root: &Path,
        branch: &str,
        added: u32,
        removed: u32,
    ) -> bool {
        let Some(status) = self.status.get(host, root) else {
            return false;
        };
        if status.branch == branch && status.added == added && status.removed == removed {
            return false;
        }
        self.status.insert(
            host,
            root.to_path_buf(),
            GitStatus {
                branch: branch.to_string(),
                added,
                removed,
            },
        );
        true
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
    fn a_diff_read_corrects_the_counts() {
        let mut cache = GitStatusCache::default();
        let cwd = Path::new("/repo/sub");
        cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((27, 8)))));

        assert!(cache.note_diff_read(L, Path::new("/repo"), "main", 3, 1));
        let got = cache.status_for(L, cwd).unwrap();
        assert_eq!((got.added, got.removed), (3, 1));
        assert_eq!(got.branch, "main");

        // Nothing to say when the diff agrees with what is already there.
        assert!(!cache.note_diff_read(L, Path::new("/repo"), "main", 3, 1));
    }

    /// A branch switched outside tty7 leaves the cached status naming the old
    /// one. The diff overlay compares its snapshot's branch against this cache
    /// to decide whether the repository moved under it, so a disagreement the
    /// diff read cannot settle is a re-probe that never stops. The read settles
    /// it: the second call has nothing left to say.
    #[test]
    fn a_diff_read_settles_a_branch_the_cache_has_stale() {
        let mut cache = GitStatusCache::default();
        let cwd = Path::new("/repo");
        cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((0, 0)))));

        assert!(cache.note_diff_read(L, cwd, "feat/x", 0, 0));
        assert_eq!(cache.status_for(L, cwd).unwrap().branch, "feat/x");
        assert!(
            !cache.note_diff_read(L, cwd, "feat/x", 0, 0),
            "the same answer twice is not news — this is what breaks the loop"
        );
    }

    #[test]
    fn a_diff_for_a_repo_nobody_probed_is_dropped() {
        // The counts hang off a root the probe established. Without one there
        // is no row to correct, and inventing an entry would leave it with no
        // branch to show.
        let mut cache = GitStatusCache::default();
        assert!(!cache.note_diff_read(L, Path::new("/elsewhere"), "main", 3, 1));
        assert!(cache.status_for(L, Path::new("/elsewhere")).is_none());
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
    fn a_linked_worktrees_root_is_not_its_home() {
        // `known_repo_for` groups a worktree with the repository it belongs
        // to; `repo_root_for` answers with the working tree itself, which is
        // the key every git-shaped cache is stored under.
        let mut cache = GitStatusCache::default();
        let wt = Path::new("/repo/.wt/feat");
        cache.finish_probe(L, wt, Some(wt_snap("/repo/.wt/feat", "/repo", "feat/x")));

        assert_eq!(
            cache.repo_root_for(L, wt),
            Some(Path::new("/repo/.wt/feat"))
        );
        assert_eq!(
            cache.known_repo_for(L, wt),
            Some(Some(PathBuf::from("/repo")))
        );

        let plain = Path::new("/tmp/notes");
        cache.finish_probe(L, plain, None);
        assert_eq!(cache.repo_root_for(L, plain), None, "not a repository");
        assert_eq!(cache.repo_root_for(L, Path::new("/never")), None);
    }

    #[test]
    fn clearing_a_host_leaves_the_others_alone() {
        let mut cache = GitStatusCache::default();
        let gone = HostId::from_connection_key("ssh-direct:me@box:22");
        let cwd = Path::new("/src/app");
        cache.finish_probe(L, cwd, Some(snap("/src/app", "main", Some((1, 2)))));
        cache.finish_probe(gone, cwd, Some(snap("/src/app", "feat/x", Some((3, 4)))));

        cache.clear_host(gone);

        assert_eq!(cache.status_for(gone, cwd), None);
        assert_eq!(cache.known_repo_for(gone, cwd), None);
        assert!(
            cache.begin_probe_throttled(gone, cwd, Duration::from_secs(60)),
            "a reconnect must be free to ask again straight away"
        );
        assert_eq!(cache.status_for(L, cwd).unwrap().branch, "main");
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

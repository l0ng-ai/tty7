use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use crate::core::git::{GitStatus, RepoSnapshot, probe};
use crate::ui::host_ops::{ByHost, HostId, InFlight};

/// The spelling a directory is keyed by in here.
///
/// This cache is where a repository gets its identity: `roots` maps a cwd to a
/// root, and `homes`, `status` and `last_probe` are then all keyed by that
/// root, which is in turn the key `ScmData` and the diff overlay use. The two
/// halves of a lookup arrive from different places — a cwd from the pane, a
/// root from `git`, and either one possibly past `fs::canonicalize` — so this
/// is the one place that has to insist they agree. Off Windows, and for every
/// path that already spells itself the OS's way, it is a borrow and nothing
/// else. See [`tty7_core::core::path_spelling`].
fn key(path: &Path) -> Cow<'_, Path> {
    tty7_core::core::path_spelling::local_spelling(path)
}

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
        let root = self.roots.get(host, &*key(cwd))?.as_ref()?;
        self.status.get(host, root.as_path()).cloned()
    }

    pub fn known_repo_for(&self, host: HostId, cwd: &Path) -> Option<Option<PathBuf>> {
        let root = self.roots.get(host, &*key(cwd))?;
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
        self.roots.get(host, &*key(cwd))?.as_deref()
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
        let key = (host, key(cwd).into_owned());
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
        let cwd = key(cwd);
        if self.probes.is_pending(&(host, cwd.to_path_buf())) {
            return false;
        }
        let throttle = self.throttle_key(host, &cwd).to_path_buf();
        if self
            .last_probe
            .get(host, throttle.as_path())
            .is_some_and(|at| at.elapsed() < min_interval)
        {
            return false;
        }
        self.last_probe.insert(host, throttle, Instant::now());
        self.probes.begin((host, cwd.into_owned()));
        true
    }

    /// `cwd` is already in the cache's own spelling — every caller of this one
    /// has been past [`key`].
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
    ///
    /// `counts` is `None` when the diff that was read is not the one these
    /// numbers are: they mean `git diff --numstat HEAD`, so only a snapshot of
    /// HEAD is comparable to them. An unstaged or staged patch is a different
    /// question with a smaller answer, and publishing it as if it were this one
    /// silently wrong-footed the sidebar — opening an untracked file from the
    /// Source Control panel takes the worktree's source, and a repository with
    /// two lines staged went from `+8 −6` to `+6 −6` on the click. The branch
    /// still lands: it is the same answer whatever was diffed.
    pub fn note_diff_read(
        &mut self,
        host: HostId,
        root: &Path,
        branch: &str,
        counts: Option<(u32, u32)>,
    ) -> bool {
        let root = key(root);
        let Some(status) = self.status.get(host, &*root) else {
            return false;
        };
        let (added, removed) = counts.unwrap_or((status.added, status.removed));
        if status.branch == branch && status.added == added && status.removed == removed {
            return false;
        }
        self.status.insert(
            host,
            root.into_owned(),
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
        // A snapshot arrives spelled by `git`, the cwd by whoever asked for
        // the probe. Both land in the cache's own spelling or the root a
        // status is filed under is not the root the next lookup asks for.
        let cwd = key(cwd);
        let snapshot = snapshot.map(|snap| RepoSnapshot {
            root: key(&snap.root).into_owned(),
            home: key(&snap.home).into_owned(),
            ..snap
        });
        let rerun = !self.probes.finish(&(host, cwd.to_path_buf()));
        let throttle = match &snapshot {
            Some(snap) => snap.root.clone(),
            None => self.throttle_key(host, &cwd).to_path_buf(),
        };
        self.last_probe.insert(host, throttle, Instant::now());
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

        assert!(cache.note_diff_read(L, Path::new("/repo"), "main", Some((3, 1))));
        let got = cache.status_for(L, cwd).unwrap();
        assert_eq!((got.added, got.removed), (3, 1));
        assert_eq!(got.branch, "main");

        // Nothing to say when the diff agrees with what is already there.
        assert!(!cache.note_diff_read(L, Path::new("/repo"), "main", Some((3, 1))));
    }

    /// These counts mean `git diff --numstat HEAD`. A worktree or staged patch
    /// answers a smaller question, so it may correct the branch and must leave
    /// the numbers alone — opening an untracked file from the Source Control
    /// panel takes the worktree's source, and used to knock the staged lines
    /// off the sidebar's total on the way past.
    #[test]
    fn a_diff_of_something_other_than_head_leaves_the_counts_alone() {
        let mut cache = GitStatusCache::default();
        let cwd = Path::new("/repo");
        cache.finish_probe(L, cwd, Some(snap("/repo", "main", Some((8, 6)))));

        assert!(!cache.note_diff_read(L, cwd, "main", None), "nothing moved");
        let got = cache.status_for(L, cwd).unwrap();
        assert_eq!(
            (got.added, got.removed),
            (8, 6),
            "the worktree's own +6 −6 is not this number"
        );

        // The branch still lands: it is the same answer whatever was diffed.
        assert!(cache.note_diff_read(L, cwd, "feat/x", None));
        let got = cache.status_for(L, cwd).unwrap();
        assert_eq!(got.branch, "feat/x");
        assert_eq!((got.added, got.removed), (8, 6));
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

        assert!(cache.note_diff_read(L, cwd, "feat/x", Some((0, 0))));
        assert_eq!(cache.status_for(L, cwd).unwrap().branch, "feat/x");
        assert!(
            !cache.note_diff_read(L, cwd, "feat/x", Some((0, 0))),
            "the same answer twice is not news — this is what breaks the loop"
        );
    }

    #[test]
    fn a_diff_for_a_repo_nobody_probed_is_dropped() {
        // The counts hang off a root the probe established. Without one there
        // is no row to correct, and inventing an entry would leave it with no
        // branch to show.
        let mut cache = GitStatusCache::default();
        assert!(!cache.note_diff_read(L, Path::new("/elsewhere"), "main", Some((3, 1))));
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

    /// Every way one directory can be spelled on the way into this cache is
    /// one key.
    ///
    /// Ungated on purpose. The spellings below are the ones Windows actually
    /// produces — a pane says `C:\repo`, `git rev-parse` says `C:/repo`,
    /// `fs::canonicalize` says `\\?\C:\repo` — and on Unix they collapse to
    /// one, so this costs nothing there and is the whole test here. Gating it
    /// to unix is what let the divergence live: the *only* platform that has
    /// three spellings was the only one not running the comparison.
    #[test]
    fn one_directory_spelled_three_ways_is_one_repository() {
        let mut cache = GitStatusCache::default();
        let (a, b, c) = match cfg!(windows) {
            true => (r"C:\code\repo", "C:/code/repo", r"\\?\C:\code\repo"),
            false => ("/code/repo", "/code/repo", "/code/repo"),
        };
        let (a, b, c) = (Path::new(a), Path::new(b), Path::new(c));

        // Probed under the resolved spelling, which is what a caller that went
        // through `Host::canonicalize` has.
        cache.finish_probe(L, c, Some(snap(c.to_str().unwrap(), "main", Some((9, 9)))));

        for spelling in [a, b, c] {
            assert_eq!(
                cache.repo_root_for(L, spelling),
                Some(a),
                "{spelling:?} names the repository the others do"
            );
            assert_eq!(
                cache.known_repo_for(L, spelling),
                Some(Some(a.to_path_buf())),
                "{spelling:?}"
            );
            assert_eq!(
                cache.status_for(L, spelling).unwrap().branch,
                "main",
                "{spelling:?}"
            );
        }
    }

    /// The diff overlay's spin, in the cache underneath it.
    ///
    /// `install_diff_snapshot` hands the branch it just read back with the
    /// root `git rev-parse` printed, while the status it is correcting was
    /// filed under the root whoever probed had. When those two spellings miss
    /// each other the correction is dropped, the overlay's next
    /// `maybe_refresh` finds the same disagreement it just tried to settle,
    /// and it re-reads the diff — `load=ready loading=true`, two `git`
    /// processes a lap, for as long as the overlay is open.
    #[test]
    fn a_diff_read_settles_a_branch_it_learned_the_root_of_from_git() {
        let mut cache = GitStatusCache::default();
        let (probed, from_git) = match cfg!(windows) {
            true => (r"\\?\C:\code\repo", "C:/code/repo"),
            false => ("/code/repo", "/code/repo"),
        };
        cache.finish_probe(
            L,
            Path::new(probed),
            Some(snap(probed, "a-branch-this-repo-has-left", Some((99, 99)))),
        );

        assert!(
            cache.note_diff_read(L, Path::new(from_git), "main", Some((1, 0))),
            "the correction has to land, or the overlay reprobes forever"
        );
        let got = cache.status_for(L, Path::new(probed)).unwrap();
        assert_eq!(got.branch, "main");
        assert_eq!((got.added, got.removed), (1, 0));
        assert!(
            !cache.note_diff_read(L, Path::new(from_git), "main", Some((1, 0))),
            "and the second lap has nothing left to say — this is what ends it"
        );
    }

    /// The same loop, end to end against a repository `git` actually created,
    /// because the literals above only prove the rule and not that this is the
    /// rule the real answers need.
    ///
    /// This is the shape the diff overlay runs every frame: a status filed
    /// under the cwd a probe was asked about, then a diff read filed under the
    /// root `git rev-parse` printed. No window and no gpui, so it runs
    /// everywhere the test binary does.
    #[test]
    fn a_real_repository_files_its_probe_and_its_diff_under_one_root() {
        use tty7_core::core::git::diff::{DiffRequest, probe_diff};

        let host = tty7_core::host::local::LocalHost::new();
        let dir = std::env::temp_dir().join(format!("tty7-one-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ok = host
            .git(&dir, &["init", "--quiet"])
            .is_ok_and(|o| o.success());
        if !ok {
            let _ = std::fs::remove_dir_all(&dir);
            return; // no git on this machine
        }
        for cfg in [
            ["config", "user.email", "t@x"].as_slice(),
            ["config", "user.name", "t"].as_slice(),
        ] {
            assert!(host.git(&dir, cfg).is_ok_and(|o| o.success()));
        }
        std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
        assert!(host.git(&dir, &["add", "-A"]).is_ok_and(|o| o.success()));
        assert!(
            host.git(&dir, &["commit", "--quiet", "-m", "one"])
                .is_ok_and(|o| o.success())
        );
        std::fs::write(dir.join("a.rs"), "fn main() { /* edited */ }\n").unwrap();

        // The cwd a pane reports can have been past `Host::canonicalize`; the
        // root a diff carries never has been.
        let cwd = host.canonicalize(&dir).expect("the scratch dir resolves");
        let mut cache = GitStatusCache::default();
        let snapshot = crate::core::git::probe(&*host, &cwd).expect("a repository is here");
        cache.finish_probe(L, &cwd, Some(snapshot));
        assert_eq!(
            cache.repo_root_for(L, &cwd),
            Some(cwd.as_path()),
            "the probed root is the directory the cache was asked about"
        );

        let diff = probe_diff(&*host, &cwd, &DiffRequest::default()).expect("a diff is readable");
        assert_eq!(diff.root, cwd, "and the diff names that same directory");
        // A branch switched outside tty7 is what makes this correction the
        // thing that ends the overlay's loop rather than a no-op: the read has
        // to land the first time and have nothing to say the second.
        assert!(
            cache.note_diff_read(L, &diff.root, "moved-on", Some((0, 0))),
            "a diff read filed under git's root must reach the probe's status"
        );
        assert_eq!(cache.status_for(L, &cwd).unwrap().branch, "moved-on");
        assert!(
            !cache.note_diff_read(L, &diff.root, "moved-on", Some((0, 0))),
            "and the second lap says nothing — this is what ends the loop"
        );

        let _ = std::fs::remove_dir_all(&dir);
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

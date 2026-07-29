//! [`LocalHost`] — the [`Host`] that answers with `std::fs` and a `git`
//! subprocess.
//!
//! This is the 99% path: every workspace whose files are on this machine holds
//! one, and so does the `tty7-server` process serving a *remote* workspace to
//! someone else's client. That second role is why it is written the way it is —
//! blocking, allocation-frugal, and with every semantic decision (sort order,
//! gitignore scoring, the search walk's bounds) made *here* rather than by the
//! caller, so that a remote workspace gets byte-identical answers to a local
//! one without the client and the server having to agree on anything but the
//! wire.
//!
//! Two pieces of state, both about not repeating work:
//!
//! - the compiled `.gitignore` matchers, shared across every listing rather
//!   than shuttled to a worker and back the way the file tree used to before a
//!   host existed to own them;
//! - nothing else. A host is otherwise a pure function of the filesystem.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

use crate::core::git;
use crate::core::gitignore::GitignoreChain;
use crate::host::{
    Entry, Host, HostId, MTime, Meta, Output, SearchHit, SharedHost, ShellInventory, WatchHandle,
    WatchSub, guard_off_ui,
};

/// How long changes are collected before a batch is delivered. Matched exactly
/// by the remote implementation — see [`WatchSub::events`].
const COALESCE_WINDOW: Duration = Duration::from_millis(100);

/// This machine's filesystem and git.
pub struct LocalHost {
    /// Compiled `.gitignore` matchers, keyed by the directory each came from.
    ///
    /// Behind an `Arc` as well as a `Mutex` so the watcher's coalescing thread
    /// can hold the same cache and clear it when a `.gitignore` is edited —
    /// which is the only thing that can invalidate a compiled matcher, and the
    /// watcher is the only place that finds out.
    gitignore: Arc<Mutex<GitignoreChain>>,
}

impl LocalHost {
    /// A new local host.
    ///
    /// Returns the trait object directly: nothing in the tree wants a concrete
    /// `LocalHost`, and handing one out would invite a call site to depend on
    /// something a remote host cannot do.
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> SharedHost {
        Arc::new(LocalHost {
            gitignore: Arc::new(Mutex::new(GitignoreChain::default())),
        })
    }

    /// The process-wide local host.
    ///
    /// One instance, so the gitignore cache is shared by every local workspace
    /// instead of being recompiled per tab. Workspaces take their host from
    /// here rather than constructing their own.
    pub fn shared() -> SharedHost {
        static LOCAL: OnceLock<SharedHost> = OnceLock::new();
        LOCAL.get_or_init(LocalHost::new).clone()
    }

    /// List `dir`, keeping each entry's full path — the shape `search` needs
    /// and `read_dir` throws away.
    fn list(&self, dir: &Path, root: Option<&Path>) -> io::Result<Vec<(Entry, PathBuf)>> {
        // Two passes, because the second needs a lock the first must not hold.
        // The file tree asks for every root and every expanded directory in one
        // frame, so a dozen of these run at once; holding the shared matcher
        // cache across the `readdir` syscalls would serialize work that has no
        // reason to be serial.
        let mut out: Vec<(Entry, PathBuf)> = Vec::new();
        for e in fs::read_dir(dir)?.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            // `DirEntry::file_type` is free on Unix (it comes out of `readdir`)
            // but does *not* follow links, and a link to a directory has to
            // read as a directory — that is what the tree expands and what the
            // sort puts first. So pay for the follow only on links.
            let ft = e.file_type().ok();
            let is_symlink = ft.is_some_and(|t| t.is_symlink());
            let is_dir = if is_symlink {
                // A broken link resolves to nothing: not a directory.
                fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false)
            } else {
                ft.is_some_and(|t| t.is_dir())
            };
            out.push((
                Entry {
                    name,
                    is_dir,
                    is_symlink,
                    ignored: false,
                },
                path,
            ));
        }

        // `.git` is ignored whatever the patterns say; everything else is scored
        // against the chain, and only when there is a root to bound it.
        let mut chain = self.gitignore.lock().unwrap_or_else(|e| e.into_inner());
        for (entry, path) in &mut out {
            entry.ignored = entry.name == ".git"
                || root.is_some_and(|root| chain.is_ignored(path, entry.is_dir, root));
        }
        drop(chain);

        sort_entries(&mut out);
        Ok(out)
    }
}

/// Directories first, then case-insensitive by name.
///
/// Dotfiles keep their leading dot in that ordering, so they sort before
/// letters — which is where users expect them, and what the file tree has
/// always done.
fn sort_entries(entries: &mut [(Entry, PathBuf)]) {
    entries.sort_by(|(a, _), (b, _)| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

impl Host for LocalHost {
    fn id(&self) -> HostId {
        HostId::LOCAL
    }

    fn separator(&self) -> char {
        std::path::MAIN_SEPARATOR
    }

    fn join(&self, dir: &Path, name: &str) -> PathBuf {
        // Native semantics locally: `Path::join` already knows this platform's
        // rules, including the ones a separator alone doesn't capture.
        dir.join(name)
    }

    fn is_absolute(&self, p: &Path) -> bool {
        p.is_absolute()
    }

    fn read_dir(&self, dir: &Path, root: Option<&Path>) -> io::Result<Vec<Entry>> {
        guard_off_ui();
        Ok(self.list(dir, root)?.into_iter().map(|(e, _)| e).collect())
    }

    fn stat(&self, p: &Path) -> io::Result<Meta> {
        guard_off_ui();
        // `symlink_metadata` first: it answers `is_symlink` and, for the
        // overwhelmingly common non-link, is also the answer — one syscall
        // instead of two.
        let lmd = fs::symlink_metadata(p)?;
        let is_symlink = lmd.file_type().is_symlink();
        let md = if is_symlink { fs::metadata(p)? } else { lmd };
        Ok(Meta {
            is_dir: md.is_dir(),
            is_symlink,
            len: md.len(),
            mtime: md.modified().ok().map(MTime::from_system_time),
            readonly: md.permissions().readonly(),
        })
    }

    fn read_file(&self, p: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
        guard_off_ui();
        let md = fs::metadata(p)?;
        if md.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("{} is a directory", p.display()),
            ));
        }
        // Checked before reading, not after: the whole point of the limit is
        // that an oversized file is never carried anywhere.
        if md.len() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!(
                    "{} is {} bytes, over the {max_bytes} limit",
                    p.display(),
                    md.len()
                ),
            ));
        }
        fs::read(p)
    }

    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        guard_off_ui();
        fs::canonicalize(p)
    }

    fn search(
        &self,
        roots: &[PathBuf],
        query: &str,
        limit: usize,
        max_dirs: usize,
        show_hidden: bool,
    ) -> io::Result<Vec<SearchHit>> {
        guard_off_ui();
        let needle = query.to_lowercase();
        let mut out: Vec<SearchHit> = Vec::new();
        // Shared across roots: the budget bounds the *search*, not each root,
        // so a workspace with six roots cannot walk six times as far.
        let mut visited = 0usize;
        for root in roots {
            // A deque, not a `Vec` with `remove(0)`: the frontier of a wide tree
            // gets long and shifting it down per pop is quadratic.
            let mut queue: VecDeque<PathBuf> = VecDeque::from([root.clone()]);
            while let Some(dir) = queue.pop_front() {
                if out.len() >= limit || visited >= max_dirs {
                    break;
                }
                visited += 1;
                // An unreadable directory is skipped, not fatal — a search that
                // aborted on the first permission-denied subdirectory would be
                // useless on any real machine.
                let Ok(entries) = self.list(&dir, Some(root)) else {
                    continue;
                };
                for (e, path) in entries {
                    // `.git`, `target`, `node_modules`: where the file count
                    // explodes and never where anyone is searching. Skipping
                    // them is what keeps the directory budget meaningful.
                    if !show_hidden && (e.ignored || e.name.starts_with('.')) {
                        continue;
                    }
                    if e.is_dir {
                        queue.push_back(path.clone());
                    }
                    if e.name.to_lowercase().contains(&needle) {
                        out.push(SearchHit {
                            name: e.name,
                            path,
                            is_dir: e.is_dir,
                            ignored: e.ignored,
                        });
                        if out.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    fn write_file(&self, p: &Path, bytes: &[u8]) -> io::Result<Meta> {
        guard_off_ui();
        fs::write(p, bytes)?;
        // Stat immediately after, on the same thread that just wrote: the
        // remote peer answers from the same place, so both hosts report the
        // metadata the write itself produced rather than whatever a later
        // caller happens to observe.
        self.stat(p)
    }

    fn create_file_new(&self, p: &Path) -> io::Result<()> {
        guard_off_ui();
        fs::File::create_new(p).map(|_| ())
    }

    fn create_dir(&self, p: &Path, recursive: bool) -> io::Result<()> {
        guard_off_ui();
        if recursive {
            fs::create_dir_all(p)
        } else {
            fs::create_dir(p)
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        guard_off_ui();
        // `fs::rename` overwrites silently on Unix, and this API promises it
        // doesn't. `symlink_metadata` rather than `exists` so a dangling
        // symlink at the destination still counts as occupied — clobbering one
        // would destroy a link the user can see in the tree.
        if fs::symlink_metadata(to).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", to.display()),
            ));
        }
        fs::rename(from, to)
    }

    fn remove(&self, p: &Path, recursive: bool) -> io::Result<()> {
        guard_off_ui();
        // `symlink_metadata`, so a symlink pointing at a directory is unlinked
        // rather than recursed into — deleting a link must never delete what it
        // points at.
        let md = fs::symlink_metadata(p)?;
        if md.is_dir() {
            if recursive {
                fs::remove_dir_all(p)
            } else {
                fs::remove_dir(p)
            }
        } else {
            fs::remove_file(p)
        }
    }

    fn repo_root(&self, p: &Path) -> io::Result<Option<PathBuf>> {
        guard_off_ui();
        // `.git` is a directory in a normal checkout and a *file* in a linked
        // worktree, so test for existence rather than for a directory.
        Ok(p.ancestors()
            .find(|a| a.join(".git").exists())
            .map(Path::to_path_buf))
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> io::Result<Output> {
        guard_off_ui();
        git::git_output(cwd, args)
    }

    /// Straight off the pipe: this machine's git writes into a buffer we drain
    /// as it fills, so a multi-megabyte diff never exists as one allocation.
    fn git_lines(
        &self,
        cwd: &Path,
        args: &[&str],
        on_line: &mut dyn FnMut(&str),
    ) -> io::Result<Option<i32>> {
        guard_off_ui();
        let mut split = git::LineSplitter::default();
        let code = git::git_stream(cwd, args, |chunk| {
            split.push(chunk, &mut *on_line);
            true
        })?;
        split.finish(&mut *on_line);
        Ok(code)
    }

    fn shells(&self) -> io::Result<ShellInventory> {
        guard_off_ui();
        Ok(crate::core::shells::inventory())
    }

    fn watch(&self, dirs: &[PathBuf]) -> io::Result<WatchSub> {
        guard_off_ui();
        local_watch(dirs, Arc::clone(&self.gitignore))
    }
}

// ---------------------------------------------------------------------------
// Watching
// ---------------------------------------------------------------------------

/// The watched set, in both the form the caller gave and the form the platform
/// reports events in.
///
/// macOS' FSEvents canonicalizes: a watch on `/var/folders/…` reports
/// `/private/var/folders/…`. Without the second form every event would look
/// like it came from somewhere unwatched; without the first, callers would get
/// back paths they cannot match against the ones they asked about. So both are
/// kept and events are rewritten into the caller's vocabulary on the way out.
#[derive(Default)]
struct WatchedDirs {
    /// Canonical form → the form the caller used.
    by_canonical: HashMap<PathBuf, PathBuf>,
    /// Exactly what the caller asked for, for `set_dirs` diffing.
    given: HashSet<PathBuf>,
}

impl WatchedDirs {
    /// The caller-facing path for an event on `p`, or `None` when `p` is not in
    /// (or directly under) a watched directory.
    ///
    /// This filter is what makes the subscription non-recursive regardless of
    /// backend: FSEvents is inherently recursive and notify only filters on a
    /// best-effort basis, so the guarantee is enforced here rather than
    /// assumed.
    fn translate(&self, p: &Path) -> Option<PathBuf> {
        if let Some(parent) = p.parent()
            && let Some(given) = self.by_canonical.get(parent)
        {
            return match p.file_name() {
                Some(name) => Some(given.join(name)),
                None => Some(given.clone()),
            };
        }
        // The watched directory itself (created, removed, renamed).
        self.by_canonical.get(p).cloned()
    }
}

/// A live local watch: the notify watcher plus the set it is following.
struct LocalWatch {
    inner: Mutex<LocalWatchInner>,
    /// The delivery end, kept solely so dropping this handle can close it.
    ///
    /// Tearing the watcher down is not instantaneous — the OS backend has its
    /// own thread, and on Windows a `ReadDirectoryChangesW` completion can fire
    /// *during* teardown, reach the event closure while `raw_tx` is still
    /// alive, and be forwarded by a coalescer that has not noticed the
    /// disconnect yet. A consumer holding a clone of the receiver would then
    /// see an event for a change made after it unsubscribed.
    ///
    /// Closing the channel here makes "dropped" mean "no further batches" at
    /// the instant of the drop, whatever the backend does afterwards. Batches
    /// already queued stay readable — `close` stops sends, not receives — which
    /// is the one thing a consumer racing its own drop may legitimately still
    /// see.
    batch_tx: smol::channel::Sender<Vec<PathBuf>>,
}

impl Drop for LocalWatch {
    fn drop(&mut self) {
        self.batch_tx.close();
    }
}

struct LocalWatchInner {
    watcher: notify::RecommendedWatcher,
    dirs: Arc<Mutex<WatchedDirs>>,
}

impl WatchHandle for LocalWatch {
    fn set_dirs(&self, dirs: &[PathBuf]) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let want: HashSet<PathBuf> = dirs.iter().cloned().collect();
        let LocalWatchInner { watcher, dirs } = &mut *inner;
        let mut set = dirs.lock().unwrap_or_else(|e| e.into_inner());

        for gone in set.given.difference(&want) {
            let _ = watcher.unwatch(gone);
        }
        let added: Vec<PathBuf> = want.difference(&set.given).cloned().collect();
        // Rebuild rather than patch: `by_canonical` is keyed by a form we do not
        // hold the inverse of, and the set is at most a few dozen expanded
        // directories.
        set.by_canonical.clear();
        for d in &added {
            // A directory that has just been deleted is not an error worth
            // failing the whole re-subscription over — the next listing will
            // notice it is gone.
            let _ = watcher.watch(d, RecursiveMode::NonRecursive);
        }
        for d in &want {
            let canon = fs::canonicalize(d).unwrap_or_else(|_| d.clone());
            set.by_canonical.insert(canon, d.clone());
            set.by_canonical.insert(d.clone(), d.clone());
        }
        set.given = want;
        Ok(())
    }
}

/// Build a watch over `dirs`, coalescing events into 100ms batches.
///
/// `gitignore` is cleared whenever a batch contains a `.gitignore`, which is the
/// only event that can invalidate a compiled matcher. Doing it here rather than
/// asking callers to remember means a remote client gets the same invalidation
/// for free: the server's own host is the one watching.
fn local_watch(dirs: &[PathBuf], gitignore: Arc<Mutex<GitignoreChain>>) -> io::Result<WatchSub> {
    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();
    let (batch_tx, batch_rx) = smol::channel::unbounded::<Vec<PathBuf>>();
    let watched: Arc<Mutex<WatchedDirs>> = Arc::new(Mutex::new(WatchedDirs::default()));

    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && !ev.paths.is_empty()
        {
            // A closed receiver means the subscription was dropped; the watcher
            // is on its way out too, so there is nothing to report.
            let _ = raw_tx.send(ev.paths);
        }
    })
    .map_err(notify_to_io)?;

    let handle = LocalWatch {
        inner: Mutex::new(LocalWatchInner {
            watcher,
            dirs: Arc::clone(&watched),
        }),
        batch_tx: batch_tx.clone(),
    };
    handle.set_dirs(dirs)?;

    // The coalescer. It ends when the watcher is dropped: that drops the event
    // closure, which drops `raw_tx`, which disconnects this receiver.
    std::thread::Builder::new()
        .name("tty7-host-watch".into())
        .spawn(move || coalesce(raw_rx, batch_tx, watched, gitignore))
        .map_err(|e| io::Error::other(format!("watch thread: {e}")))?;

    Ok(WatchSub::new(batch_rx, Box::new(handle)))
}

/// Collect raw events into deduplicated 100ms batches.
///
/// The window exists so that a `cargo build` touching ten thousand files is one
/// repaint rather than ten thousand, and it is identical on the remote side so
/// that where the files live cannot change how the tree behaves.
fn coalesce(
    raw_rx: std::sync::mpsc::Receiver<Vec<PathBuf>>,
    batch_tx: smol::channel::Sender<Vec<PathBuf>>,
    watched: Arc<Mutex<WatchedDirs>>,
    gitignore: Arc<Mutex<GitignoreChain>>,
) {
    loop {
        // Block until something happens at all — an idle watch costs nothing.
        let Ok(first) = raw_rx.recv() else { return };
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut batch: Vec<PathBuf> = Vec::new();
        let take = |paths: Vec<PathBuf>, batch: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>| {
            let set = watched.lock().unwrap_or_else(|e| e.into_inner());
            for p in paths {
                if let Some(translated) = set.translate(&p)
                    && seen.insert(translated.clone())
                {
                    batch.push(translated);
                }
            }
        };
        take(first, &mut batch, &mut seen);

        let deadline = Instant::now() + COALESCE_WINDOW;
        loop {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match raw_rx.recv_timeout(left) {
                Ok(paths) => take(paths, &mut batch, &mut seen),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                // Watcher gone: deliver what we have, then stop.
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    if !batch.is_empty() {
                        let _ = batch_tx.send_blocking(batch);
                    }
                    return;
                }
            }
        }

        if batch.is_empty() {
            continue;
        }
        // A `.gitignore` edit changes the answer for every path under it, so the
        // compiled matchers all go. Cheap: they recompile lazily, per directory,
        // on the next listing that needs one.
        if batch
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == ".gitignore"))
        {
            gitignore.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
        if batch_tx.send_blocking(batch).is_err() {
            return;
        }
    }
}

/// notify's error type carries an `io::Error` for the cases that have one; the
/// rest become `Other` with the message preserved.
fn notify_to_io(e: notify::Error) -> io::Error {
    match e.kind {
        notify::ErrorKind::Io(io) => io,
        other => io::Error::other(format!("watch: {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::conformance::Sandbox;

    /// A temp directory that satisfies the conformance sandbox contract.
    struct TempSandbox(tempfile::TempDir);

    impl Sandbox for TempSandbox {
        fn path(&self) -> &Path {
            self.0.path()
        }

        fn symlink(&self, target: &Path, link: &Path) -> Option<io::Result<()>> {
            #[cfg(unix)]
            {
                Some(std::os::unix::fs::symlink(target, link))
            }
            #[cfg(not(unix))]
            {
                // Windows needs a privilege we cannot assume in CI; the cases
                // that want a symlink skip instead of failing.
                let _ = (target, link);
                None
            }
        }
    }

    fn sandbox() -> (SharedHost, TempSandbox) {
        (
            LocalHost::new(),
            TempSandbox(tempfile::TempDir::new().unwrap()),
        )
    }

    // Every case in the shared suite, run against `LocalHost`. `RemoteHost` and
    // the stdio server run the identical list; a divergence between them is
    // exactly what this exists to catch.
    crate::host_conformance_suite!(local, sandbox);

    /// The sort is the file tree's, verbatim: directories first, then
    /// case-insensitively by name, with dotfiles keeping their leading dot (so
    /// `.gitignore` sorts before `main.rs`).
    #[test]
    fn sort_matches_the_file_trees_order() {
        let mut v: Vec<(Entry, PathBuf)> = ["main.rs", "Cargo.toml", ".gitignore", "src", "Zeta"]
            .iter()
            .map(|n| {
                (
                    Entry {
                        name: (*n).to_string(),
                        is_dir: *n == "src",
                        is_symlink: false,
                        ignored: false,
                    },
                    PathBuf::from(n),
                )
            })
            .collect();
        sort_entries(&mut v);
        let names: Vec<&str> = v.iter().map(|(e, _)| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["src", ".gitignore", "Cargo.toml", "main.rs", "Zeta"]
        );
    }

    /// The process-wide host is one instance, so every local workspace shares
    /// the gitignore cache rather than recompiling per tab.
    #[test]
    fn shared_is_a_singleton() {
        assert!(Arc::ptr_eq(&LocalHost::shared(), &LocalHost::shared()));
        assert!(LocalHost::shared().id().is_local());
    }

    /// The gitignore chain the file tree used to hand back and forth now lives
    /// in the host — and scores the same fixture the same way: deepest match
    /// wins, `!` un-ignores, `.git` is ignored whatever the patterns say.
    #[test]
    fn gitignore_chain_scores_the_file_tree_fixture() {
        let (h, tmp) = sandbox();
        let root = tmp.path();
        h.create_dir(&root.join(".git"), false).unwrap();
        h.create_dir(&root.join("src"), false).unwrap();
        h.write_file(&root.join(".gitignore"), b"*.log\nbuild/\n")
            .unwrap();
        h.write_file(&root.join("src/.gitignore"), b"!keep.log\n")
            .unwrap();
        h.write_file(&root.join("drop.log"), b"").unwrap();
        h.write_file(&root.join("src/keep.log"), b"").unwrap();
        h.write_file(&root.join("src/main.rs"), b"").unwrap();

        let ignored = |entries: &[Entry], name: &str| {
            entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
                .ignored
        };
        let top = h.read_dir(root, Some(root)).unwrap();
        assert!(ignored(&top, "drop.log"));
        assert!(ignored(&top, ".git"));
        assert!(!ignored(&top, "src"));

        let nested = h.read_dir(&root.join("src"), Some(root)).unwrap();
        assert!(!ignored(&nested, "keep.log"), "whitelist un-ignores");
        assert!(!ignored(&nested, "main.rs"));

        // And the search agrees with the listing: the ignored `.log` stays out,
        // the whitelisted one comes back.
        let hits = h
            .search(&[root.to_path_buf()], "log", 200, 2000, false)
            .unwrap();
        let names: Vec<&str> = hits.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["keep.log"]);
    }

    /// Editing a `.gitignore` has to change the answer, and the only thing that
    /// finds out is the watcher — so the invalidation rides along with it.
    #[test]
    fn a_gitignore_edit_through_the_watcher_clears_the_cache() {
        let (h, tmp) = sandbox();
        let root = tmp.path().to_path_buf();
        h.write_file(&root.join(".gitignore"), b"*.log\n").unwrap();
        h.write_file(&root.join("a.log"), b"").unwrap();
        let listed = h.read_dir(&root, Some(&root)).unwrap();
        assert!(listed.iter().any(|e| e.name == "a.log" && e.ignored));

        let sub = h.watch(&[root.clone()]).unwrap();

        // Poll rather than block on the channel, and re-write each round.
        // FSEvents registers asynchronously, so a write landing in the first
        // few milliseconds after `watch` can simply never be reported — and a
        // test that blocked waiting for that event would hang forever rather
        // than fail.
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut cleared = false;
        while Instant::now() < deadline {
            h.write_file(&root.join(".gitignore"), b"# nothing\n")
                .unwrap();
            std::thread::sleep(Duration::from_millis(250));
            while sub.events().try_recv().is_ok() {}
            if h.read_dir(&root, Some(&root))
                .unwrap()
                .iter()
                .any(|e| e.name == "a.log" && !e.ignored)
            {
                cleared = true;
                break;
            }
        }
        assert!(
            cleared,
            "a `.gitignore` change seen by the watcher must drop the compiled matchers"
        );
    }
}

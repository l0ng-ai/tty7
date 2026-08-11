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

const COALESCE_WINDOW: Duration = Duration::from_millis(100);

/// What every git we spawn runs with, on top of what `git_output_with_env`
/// already sets: nothing may stop and ask a human anything.
///
/// It only bites when git reaches for a credential, which in practice is
/// `fetch`/`pull`/`push` — `status`, `diff` and `log` have nothing to prompt
/// about, so carrying it on the read path too costs nothing and buys the one
/// thing we cannot get any other way: the wire carries a single `Git` request
/// with no "this one talks to a network" bit, so the remote `tty7-server`
/// arrives here with the same args and is protected by the same rule, without a
/// protocol change.
///
/// `git_output_with_env` already nulls stdin, and that is not enough — with no
/// `GIT_TERMINAL_PROMPT` git opens `/dev/tty` directly and blocks on it, which
/// is exactly the hang this prevents.
const NO_PROMPT_ENV: &[(&str, Option<&str>)] = &[
    // Fail instead of blocking on `Username for 'https://…'`.
    ("GIT_TERMINAL_PROMPT", Some("0")),
    // Nobody is watching for a password dialog a background probe popped up.
    ("GIT_ASKPASS", None),
    ("SSH_ASKPASS", None),
    // OpenSSH 8.4+; without it ssh may fall back to askpass on its own.
    ("SSH_ASKPASS_REQUIRE", Some("never")),
];

/// `NO_PROMPT_ENV`, plus a batch-mode ssh unless the user picked their own
/// `GIT_SSH_COMMAND` — replacing theirs would drop the identity file or jump
/// host they configured. `BatchMode=yes` bans only interactive password and
/// passphrase prompts; a key held by ssh-agent still authenticates.
///
/// A repository's `core.sshCommand` does lose to this, because that is git's
/// own precedence. Honouring it would mean a `git config` probe before every
/// call, and this is the read path too.
fn no_prompt_env() -> Vec<(&'static str, Option<&'static str>)> {
    let mut env = NO_PROMPT_ENV.to_vec();
    // `GIT_SSH` too: it is the older spelling of the same choice (plink on
    // Windows, most commonly), and `GIT_SSH_COMMAND` outranks it — forcing
    // ours would silently swap their transport out.
    if std::env::var_os("GIT_SSH_COMMAND").is_none() && std::env::var_os("GIT_SSH").is_none() {
        env.push(("GIT_SSH_COMMAND", Some("ssh -o BatchMode=yes")));
    }
    env
}

pub struct LocalHost {
    gitignore: Arc<Mutex<GitignoreChain>>,
}

impl LocalHost {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> SharedHost {
        Arc::new(LocalHost {
            gitignore: Arc::new(Mutex::new(GitignoreChain::default())),
        })
    }

    pub fn shared() -> SharedHost {
        static LOCAL: OnceLock<SharedHost> = OnceLock::new();
        LOCAL.get_or_init(LocalHost::new).clone()
    }

    fn list(&self, dir: &Path, root: Option<&Path>) -> io::Result<Vec<(Entry, PathBuf)>> {
        let mut out: Vec<(Entry, PathBuf)> = Vec::new();
        for e in fs::read_dir(dir)?.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let ft = e.file_type().ok();
            let is_symlink = ft.is_some_and(|t| t.is_symlink());
            let is_dir = if is_symlink {
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
        let mut visited = 0usize;
        for root in roots {
            let mut queue: VecDeque<PathBuf> = VecDeque::from([root.clone()]);
            while let Some(dir) = queue.pop_front() {
                if out.len() >= limit || visited >= max_dirs {
                    break;
                }
                visited += 1;
                let Ok(entries) = self.list(&dir, Some(root)) else {
                    continue;
                };
                for (e, path) in entries {
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
        Ok(p.ancestors()
            .find(|a| a.join(".git").exists())
            .map(Path::to_path_buf))
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> io::Result<Output> {
        guard_off_ui();
        git::git_output_with_env(cwd, args, &no_prompt_env())
    }

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

#[derive(Default)]
struct WatchedDirs {
    by_canonical: HashMap<PathBuf, PathBuf>,
    given: HashSet<PathBuf>,
}

impl WatchedDirs {
    fn translate(&self, p: &Path) -> Option<PathBuf> {
        if let Some(parent) = p.parent()
            && let Some(given) = self.by_canonical.get(parent)
        {
            return match p.file_name() {
                Some(name) => Some(given.join(name)),
                None => Some(given.clone()),
            };
        }
        self.by_canonical.get(p).cloned()
    }
}

struct LocalWatch {
    inner: Mutex<LocalWatchInner>,
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
        set.by_canonical.clear();
        for d in &added {
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

fn local_watch(dirs: &[PathBuf], gitignore: Arc<Mutex<GitignoreChain>>) -> io::Result<WatchSub> {
    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();
    let (batch_tx, batch_rx) = smol::channel::unbounded::<Vec<PathBuf>>();
    let watched: Arc<Mutex<WatchedDirs>> = Arc::new(Mutex::new(WatchedDirs::default()));

    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && !ev.paths.is_empty()
            && crate::host::is_content_change(&ev.kind)
        {
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

    std::thread::Builder::new()
        .name("tty7-host-watch".into())
        .spawn(move || coalesce(raw_rx, batch_tx, watched, gitignore))
        .map_err(|e| io::Error::other(format!("watch thread: {e}")))?;

    Ok(WatchSub::new(batch_rx, Box::new(handle)))
}

fn coalesce(
    raw_rx: std::sync::mpsc::Receiver<Vec<PathBuf>>,
    batch_tx: smol::channel::Sender<Vec<PathBuf>>,
    watched: Arc<Mutex<WatchedDirs>>,
    gitignore: Arc<Mutex<GitignoreChain>>,
) {
    loop {
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

    crate::host_conformance_suite!(local, sandbox);

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

    #[test]
    fn shared_is_a_singleton() {
        assert!(Arc::ptr_eq(&LocalHost::shared(), &LocalHost::shared()));
        assert!(LocalHost::shared().id().is_local());
    }

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

        let hits = h
            .search(&[root.to_path_buf()], "log", 200, 2000, false)
            .unwrap();
        let names: Vec<&str> = hits.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["keep.log"]);
    }

    #[test]
    fn a_gitignore_edit_through_the_watcher_clears_the_cache() {
        let (h, tmp) = sandbox();
        let root = tmp.path().to_path_buf();
        h.write_file(&root.join(".gitignore"), b"*.log\n").unwrap();
        h.write_file(&root.join("a.log"), b"").unwrap();
        let listed = h.read_dir(&root, Some(&root)).unwrap();
        assert!(listed.iter().any(|e| e.name == "a.log" && e.ignored));

        let sub = h.watch(&[root.clone()]).unwrap();

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

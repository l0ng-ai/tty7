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
    ContentLimits, ContentQuery, ContentResults, Entry, Host, HostId, MTime, Meta, Output,
    SearchHit, SharedHost, ShellInventory, WatchHandle, WatchSub, guard_off_ui,
};

const COALESCE_WINDOW: Duration = Duration::from_millis(100);

/// How often a directory on a WSL distro's share is re-read, since nothing
/// there says when it changed (#942).
const WSL_POLL_INTERVAL: Duration = Duration::from_secs(2);

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

    /// The real path behind `p`, in the spelling the rest of tty7 keys by.
    ///
    /// `fs::canonicalize` answers with the extended-length form on Windows —
    /// `\\?\C:\Users\x\repo` — which is a different `Prefix` component from
    /// the `C:\Users\x\repo` a shell, a pane cwd and `git` all report, and so
    /// compares unequal, hashes differently and fails `starts_with` against
    /// every one of them. `\\?\` is a Win32 API escape hatch rather than part
    /// of the path's identity, so it comes off here, at the one boundary that
    /// produces it. What the call is actually *for* — resolving a junction, a
    /// `subst` drive, an 8.3 short name or a symlink — is untouched. See
    /// [`crate::core::path_spelling`].
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        guard_off_ui();
        Ok(crate::core::path_spelling::local_spelling_buf(
            fs::canonicalize(p)?,
        ))
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

    fn search_content(
        &self,
        roots: &[PathBuf],
        query: &ContentQuery,
        limits: &ContentLimits,
    ) -> io::Result<ContentResults> {
        guard_off_ui();
        crate::host::content_search::search(roots, query, limits)
    }

    /// Replaces `p` with `bytes` atomically where it can, so a crash, a full
    /// disk or a killed process halfway through a save leaves either the old
    /// file or the new one and never a truncated mix — see [`write_replacing`]
    /// for when it cannot and writes in place instead.
    fn write_file(&self, p: &Path, bytes: &[u8]) -> io::Result<Meta> {
        guard_off_ui();
        write_replacing(p, bytes, &mut |f| io::Write::write_all(f, bytes))?;
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

    fn repo_head(&self, cwd: &Path) -> Option<Option<git::head::RepoHead>> {
        guard_off_ui();
        Some(git::head::read_head(cwd))
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

    fn agent_sessions(
        &self,
        known_dirs: &[PathBuf],
    ) -> io::Result<Vec<crate::core::agent_history::PastSession>> {
        guard_off_ui();
        use crate::core::agent_history::{Roots, scan};
        Ok(Roots::local().map_or_else(Vec::new, |roots| scan(&roots, known_dirs)))
    }

    fn watch(&self, dirs: &[PathBuf]) -> io::Result<WatchSub> {
        guard_off_ui();
        local_watch(dirs, Arc::clone(&self.gitignore))
    }
}

/// Writes `bytes` to `p` by filling a hidden sibling temp file and renaming it
/// over `p`, which is what makes a save atomic: `fs::write` truncates first, so
/// anything that stops it before the last byte lands destroys the user's file.
///
/// A rename swaps in a new inode, though, and that is not always invisible.
/// Wherever it would change something the user can see, this writes in place
/// exactly as `fs::write` did and gives up atomicity instead:
///
/// - `p` is not a regular file. A symlink must keep pointing where it points
///   and have its target written, not be replaced by a plain file; a
///   directory or a FIFO has to fail or be written the way it always was.
/// - `p` is read-only. The rename only needs the directory to be writable, so
///   it would quietly overwrite a file its owner protected; writing in place
///   fails with the same `PermissionDenied` it always did.
/// - (Unix) `p` has more than one hard link. The other names would keep the
///   old content, while an in-place write reaches all of them.
/// - (Unix) `p` belongs to another user. The new file would belong to us, so
///   a root-run save of someone else's file would hand it to root.
/// - The temp file cannot be created, typically because the directory is not
///   writable while the file is. If the directory is missing altogether the
///   in-place write fails too, with the same `NotFound` as before.
///
/// `fill` writes the content into the temp file; it is a parameter only so the
/// tests can make it fail halfway.
fn write_replacing(
    p: &Path,
    bytes: &[u8],
    fill: &mut dyn FnMut(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let existing = match fs::symlink_metadata(p) {
        Ok(md) => Some(md),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if existing
        .as_ref()
        .is_some_and(|md| !replace_is_invisible(md))
    {
        return fs::write(p, bytes);
    }
    let Some((tmp_path, mut tmp)) = create_temp_beside(p) else {
        return fs::write(p, bytes);
    };
    // From here on every early return must take the temp file with it, or a
    // failed save would litter the user's directory with `.tty7-….tmp` files.
    let staged = (|| {
        // A new file keeps the umask-derived mode it was created with, which
        // is what `fs::write` would have given it; an existing one keeps its
        // own.
        if let Some(md) = &existing {
            tmp.set_permissions(md.permissions())?;
        }
        fill(&mut tmp)?;
        // Without this the rename can reach the disk before the data does,
        // and a power cut leaves an empty file under the old name.
        tmp.sync_all()
    })();
    drop(tmp);
    // Staging failed — a full disk, say. The original is untouched and must
    // stay that way: writing it in place now would truncate it into exactly
    // the half-written file this function exists to prevent.
    if let Err(e) = staged {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp_path, p) {
        let _ = fs::remove_file(&tmp_path);
        // Windows refuses to rename over a file another program holds open,
        // where the in-place write it used to get succeeds, so there a failed
        // swap is not yet a failed save. Only the swap: the content was just
        // written out in full, so the disk has room for it.
        #[cfg(not(unix))]
        {
            let _ = e;
            return fs::write(p, bytes);
        }
        #[cfg(unix)]
        return Err(e);
    }
    sync_parent_dir(p);
    Ok(())
}

/// Whether swapping a new inode in for the file `md` describes would go
/// unnoticed — see [`write_replacing`] for why each of these matters.
fn replace_is_invisible(md: &fs::Metadata) -> bool {
    if !md.file_type().is_file() || md.permissions().readonly() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: `geteuid` takes nothing, touches no memory and cannot fail.
        let euid = unsafe { libc::geteuid() };
        // Being the owner, the owner's write bit is the one that decides
        // whether an in-place write is allowed; `readonly` above only catches
        // a file with no write bit at all.
        md.nlink() == 1 && md.uid() == euid && md.mode() & 0o200 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Creates a fresh hidden file next to `p` to stage its new content in. It has
/// to be in the same directory, since a rename across filesystems is not
/// atomic (and fails outright), and it must be new, so two saves racing each
/// other — or a leftover from a crash — can never share one.
fn create_temp_beside(p: &Path) -> Option<(PathBuf, fs::File)> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = p.parent()?;
    let name = p.file_name()?.to_string_lossy();
    for _ in 0..8 {
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.tty7-{}-{n}.tmp", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => return Some((tmp, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Flushes the directory entry the rename just changed, so the new name
/// survives a power cut too. Best effort: the content is already safe on disk,
/// and not every filesystem lets a directory be opened or synced.
fn sync_parent_dir(p: &Path) {
    #[cfg(unix)]
    if let Some(dir) = p.parent()
        && let Ok(d) = fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = p;
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
    /// The directories no change notification ever arrives for, re-read on a
    /// timer instead. Made the first time one is watched: most sessions never
    /// open a WSL tree, and an idle poller is still a thread.
    poller: Option<notify::PollWatcher>,
    handler: RawHandler,
    dirs: Arc<Mutex<WatchedDirs>>,
}

type RawHandler = Arc<dyn Fn(notify::Result<notify::Event>) + Send + Sync>;

/// Whether `dir` is on a WSL distro's `\\wsl$` (or `\\wsl.localhost`) share.
///
/// The share's redirector takes a `ReadDirectoryChangesW` and then never
/// reports anything, so a watch there looks healthy and is deaf: a file the
/// shell in the distro removes stays in the tree for good (#942). Nor does it
/// keep an NTFS alternate data stream as one: `Zone.Identifier` lands as a
/// file of its own named `name:Zone.Identifier`.
pub fn is_wsl_share(dir: &Path) -> bool {
    let s = dir.to_string_lossy().to_ascii_lowercase();
    let s = match s.strip_prefix(r"\\?\unc\") {
        Some(rest) => format!(r"\\{rest}"),
        None => s,
    };
    s.starts_with(r"\\wsl$\") || s.starts_with(r"\\wsl.localhost\")
}

impl LocalWatchInner {
    fn watcher_for(&mut self, dir: &Path) -> &mut dyn Watcher {
        if !is_wsl_share(dir) {
            return &mut self.watcher;
        }
        if self.poller.is_none() {
            let handler = Arc::clone(&self.handler);
            let config = notify::Config::default().with_poll_interval(WSL_POLL_INTERVAL);
            match notify::PollWatcher::new(move |res| handler(res), config) {
                Ok(poller) => self.poller = Some(poller),
                Err(e) => log::warn!("watch: no poller for {}: {e}", dir.display()),
            }
        }
        match &mut self.poller {
            Some(poller) => poller,
            None => &mut self.watcher,
        }
    }
}

impl WatchHandle for LocalWatch {
    fn set_dirs(&self, dirs: &[PathBuf]) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let want: HashSet<PathBuf> = dirs.iter().cloned().collect();
        let dirs = Arc::clone(&inner.dirs);
        let mut set = dirs.lock().unwrap_or_else(|e| e.into_inner());

        for gone in set.given.difference(&want) {
            let _ = inner.watcher_for(gone).unwatch(gone);
        }
        let added: Vec<PathBuf> = want.difference(&set.given).cloned().collect();
        set.by_canonical.clear();
        for d in &added {
            let _ = inner.watcher_for(d).watch(d, RecursiveMode::NonRecursive);
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

    // Shared by the notifying watcher and the poller, so a batch coalesces
    // events from both the same way.
    let handler: RawHandler = Arc::new(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && !ev.paths.is_empty()
            && crate::host::is_content_change(&ev.kind)
        {
            let _ = raw_tx.send(ev.paths);
        }
    });
    let watcher = notify::recommended_watcher({
        let handler = Arc::clone(&handler);
        move |res| handler(res)
    })
    .map_err(notify_to_io)?;

    let handle = LocalWatch {
        inner: Mutex::new(LocalWatchInner {
            watcher,
            poller: None,
            handler,
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
    fn wsl_shares_are_polled_and_nothing_else_is() {
        for dir in [
            r"\\wsl$\Ubuntu\home\me",
            r"\\WSL$\Ubuntu",
            r"\\wsl.localhost\Ubuntu-24.04\mnt\c",
            r"\\?\UNC\wsl$\Ubuntu\home\me",
            r"\\?\UNC\wsl.localhost\Ubuntu\home",
        ] {
            assert!(is_wsl_share(Path::new(dir)), "{dir}");
        }
        for dir in [
            r"C:\Users\me",
            r"\\server\share\wsl$\x",
            r"\\wslhost\share",
            r"\\?\C:\Users\me",
            "/home/me",
        ] {
            assert!(!is_wsl_share(Path::new(dir)), "{dir}");
        }
    }

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

    /// The names in `dir`, sorted — for spotting a temp file a save left behind.
    fn names_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn a_save_replaces_the_file_and_leaves_nothing_beside_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("doc.txt");
        let h = LocalHost::new();
        h.write_file(&f, b"first, and longer").unwrap();
        #[cfg(unix)]
        let before = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&f).unwrap());
        let meta = h.write_file(&f, b"second").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"second");
        assert_eq!(meta.len, 6);
        assert_eq!(names_in(tmp.path()), vec!["doc.txt"]);
        #[cfg(unix)]
        assert_ne!(
            std::os::unix::fs::MetadataExt::ino(&fs::metadata(&f).unwrap()),
            before,
            "the save must swap in a new file rather than truncate the old one"
        );
    }

    #[test]
    fn a_new_file_is_created_like_fs_write_would() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("fresh.txt");
        let control = tmp.path().join("control.txt");
        fs::write(&control, b"").unwrap();
        LocalHost::new().write_file(&f, b"hello").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"hello");
        assert_eq!(
            fs::metadata(&f).unwrap().permissions(),
            fs::metadata(&control).unwrap().permissions(),
            "a new file takes the umask's mode, as `fs::write` gives it"
        );
        assert_eq!(names_in(tmp.path()), vec!["control.txt", "fresh.txt"]);
    }

    #[test]
    fn a_save_into_a_missing_directory_fails_and_creates_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("gone").join("doc.txt");
        let err = LocalHost::new().write_file(&f, b"x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
        assert!(names_in(tmp.path()).is_empty());
    }

    #[test]
    fn a_failed_save_keeps_the_old_file_and_cleans_up() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("doc.txt");
        fs::write(&f, b"precious").unwrap();
        let err = write_replacing(&f, b"new content", &mut |file| {
            io::Write::write_all(file, b"new")?;
            Err(io::Error::other("disk full"))
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "disk full");
        assert_eq!(fs::read(&f).unwrap(), b"precious");
        assert_eq!(names_in(tmp.path()), vec!["doc.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_save_keeps_the_files_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("run.sh");
        fs::write(&f, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o750)).unwrap();
        LocalHost::new()
            .write_file(&f, b"#!/bin/sh\necho hi\n")
            .unwrap();
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o7777,
            0o750
        );
        assert_eq!(fs::read(&f).unwrap(), b"#!/bin/sh\necho hi\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_linked_file_is_written_in_place_so_every_name_sees_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("doc.txt");
        let other = tmp.path().join("alias.txt");
        fs::write(&f, b"old").unwrap();
        fs::hard_link(&f, &other).unwrap();
        LocalHost::new().write_file(&f, b"new").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"new");
        assert_eq!(fs::read(&other).unwrap(), b"new");
        assert_eq!(names_in(tmp.path()), vec!["alias.txt", "doc.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_written_through_not_replaced() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("real.txt");
        let link = tmp.path().join("link.txt");
        fs::write(&target, b"old").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        LocalHost::new().write_file(&link, b"new").unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(names_in(tmp.path()), vec!["link.txt", "real.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_file_still_refuses_the_save() {
        use std::os::unix::fs::PermissionsExt;
        // Root writes through any mode, so there is nothing to refuse.
        // SAFETY: `geteuid` takes nothing and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("locked.txt");
        fs::write(&f, b"keep").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o444)).unwrap();
        let err = LocalHost::new().write_file(&f, b"clobber").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert_eq!(fs::read(&f).unwrap(), b"keep");
        assert_eq!(names_in(tmp.path()), vec!["locked.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_writable_file_in_a_read_only_directory_is_written_in_place() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `geteuid` takes nothing and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("ro");
        fs::create_dir(&dir).unwrap();
        let f = dir.join("doc.txt");
        fs::write(&f, b"old").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let wrote = LocalHost::new().write_file(&f, b"new");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        wrote.unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"new");
        assert_eq!(names_in(&dir), vec!["doc.txt"]);
    }
}

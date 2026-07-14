//! SFTP engine for native-SSH panes (Workstream 5).
//!
//! One [`SftpManager`] (a process-wide singleton) rides the same tokio runtime the
//! [`SshManager`](super::SshManager) owns. It answers the daemon's SFTP control
//! messages (`SftpList` / `SftpOp` / transfer start/cancel/list) by opening an
//! SFTP-subsystem channel on a pane's already-authenticated `SshConnection` and
//! driving [`russh_sftp`] over it.
//!
//! ## Session lifecycle
//! - **One cached [`SftpSession`] per [`SshConnection`]** (keyed by
//!   [`ConnectionKey`]), reused across every pane that shares the connection.
//! - The cache stores a `Weak<SshConnection>` beside the session; a lookup reuses
//!   the session only while that weak still upgrades to the *same* live connection
//!   (`Arc::ptr_eq`). A reconnect (new connection, same key) transparently gets a
//!   fresh SFTP session.
//! - One-shot operations run through [`SftpManager::with_session`], which retries
//!   once with a freshly re-opened session **only** on a transport/channel failure
//!   — so a dead subsystem channel (while the connection itself lives) is re-opened
//!   transparently, while a logical SFTP error (permission denied, no such file)
//!   returns directly without a pointless retry.
//!
//! ## Threading
//! The server's std connection threads call the **sync** methods here
//! ([`list`](SftpManager::list) etc.), which `block_on` the SSH runtime handle.
//! Background transfers are `spawn`ed onto that runtime and report progress the
//! GUI polls via [`list_jobs`](SftpManager::list_jobs).
//!
//! ## Notes / limitations
//! - **posix-rename:** upload writes a `.tty7-upload-<rand>` temp then renames over
//!   the target. russh-sftp 2.3.0's high-level API does not expose the
//!   `posix-rename@openssh.com` extension, so the swap is a plain SFTP `rename`
//!   with a remove-then-rename fallback when the server refuses an
//!   overwrite-rename (FR-T2's intent: atomic-ish temp-file finish).
//! - Local filesystem access is the daemon process's own (same user) — fine per
//!   the spec.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::daemon::protocol::{
    SftpEntry, SftpEntryKind, SftpJobProgress, SftpJobState, SftpOp, SftpOpResult,
    SftpTransferKind, SftpTransferSpec,
};

use super::{ConnectionKey, SshConnection, SshManager};

/// Chunk size for streaming reads/writes (matches the Tabby reference, §6).
const CHUNK: usize = 256 * 1024;

/// How long a finished job's final progress lingers for the GUI to observe before
/// it is pruned from the job table.
const JOB_RETENTION: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Remote path helpers (pure) — also used by the GUI panel (`ui::sftp`).
// ---------------------------------------------------------------------------

/// Join a remote directory path with a child name, POSIX-style (`/` separator,
/// never a backslash — the remote is always POSIX regardless of the daemon's OS).
pub fn remote_join(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "/" {
        format!("/{}", name.trim_start_matches('/'))
    } else {
        format!(
            "{}/{}",
            dir.trim_end_matches('/'),
            name.trim_start_matches('/')
        )
    }
}

/// The parent directory of a remote path. Root's parent is root. Trailing slashes
/// are ignored (so `/a/b/` → `/a`).
pub fn remote_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(idx) => trimmed[..idx].to_string(),
    }
}

/// The final component (basename) of a remote path (`/a/b` → `b`, `/` → `/`).
pub fn remote_basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(idx) => trimmed[idx + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

/// The temp filename an upload writes to before renaming over its target:
/// `<remote>.tty7-upload-<rand>`. Kept in the *same directory* as the target so
/// the finishing rename is same-filesystem (atomic on the server).
pub fn upload_temp_name(remote: &str) -> String {
    // A cheap, dependency-free random suffix from the system clock + a counter.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{remote}.tty7-upload-{:x}{:x}", nanos, n)
}

/// Whether a server-supplied directory-entry `name` is safe to use as a *single*
/// local path component when building a download destination.
///
/// A recursive download turns remote entry names into local path components
/// (`lpath.join(name)`). A malicious or compromised server can return names like
/// `..`, `../../etc/foo`, or an absolute `/etc/foo`; `Path::join` with an absolute
/// component discards the base, and `..` escapes upward — arbitrary local file
/// write (CVE-2019-6111-class). Accept only a name that is exactly one *normal*
/// path component: reject empty, `.`, `..`, anything containing a `/` or `\\`
/// separator, and anything that doesn't resolve to a single `Component::Normal`.
pub fn safe_local_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    // Reject either separator on every platform: a POSIX server name must never
    // introduce a Windows path separator either.
    if name.contains('/') || name.contains('\\') {
        return false;
    }
    let mut comps = Path::new(name).components();
    matches!(
        (comps.next(), comps.next()),
        (Some(Component::Normal(_)), None)
    )
}

/// The temp path a download writes to before renaming over its target:
/// `<local>.tty7-download-<rand>`, a sibling in the *same directory* so the
/// finishing rename is same-filesystem (atomic). Mirrors [`upload_temp_name`].
fn download_temp_path(lpath: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // Append to the full path (a sibling with a suffix) so the temp stays in the
    // destination directory regardless of the file name's own extension.
    let mut os = lpath.as_os_str().to_os_string();
    os.push(format!(".tty7-download-{:x}{:x}", nanos, n));
    PathBuf::from(os)
}

// ---------------------------------------------------------------------------
// Entry classification (pure).
// ---------------------------------------------------------------------------

/// Classify a remote entry from its attributes. Symlink is checked first because
/// the SFTP type bits let a symlink also satisfy `is_regular` (S_IFLNK contains
/// the S_IFREG bit), so order matters.
fn classify(attrs: &FileAttributes) -> SftpEntryKind {
    if attrs.is_symlink() {
        SftpEntryKind::Symlink
    } else if attrs.is_dir() {
        SftpEntryKind::Dir
    } else {
        SftpEntryKind::File
    }
}

fn entry_from_attrs(name: &str, attrs: &FileAttributes) -> SftpEntry {
    SftpEntry {
        name: name.to_string(),
        kind: classify(attrs),
        size: attrs.size.unwrap_or(0),
        mtime: attrs.mtime.map(u64::from).unwrap_or(0),
        permissions: attrs.permissions.unwrap_or(0),
        target_is_dir: false,
    }
}

// ---------------------------------------------------------------------------
// Transfer job state machine (pure) — tested without any SFTP/window.
// ---------------------------------------------------------------------------

/// The mutable progress of one transfer job. Terminal states (`Done`/`Error`/
/// `Cancelled`) latch: once reached, further transitions are ignored, so a late
/// `add_bytes` after cancellation can't resurrect a job or corrupt its status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobProgress {
    pub state: SftpJobState,
    pub current: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub error: Option<String>,
}

impl JobProgress {
    pub fn new() -> Self {
        Self {
            state: SftpJobState::Running,
            current: String::new(),
            bytes_done: 0,
            bytes_total: 0,
            error: None,
        }
    }

    fn is_terminal(&self) -> bool {
        !matches!(self.state, SftpJobState::Running)
    }

    pub fn set_total(&mut self, total: u64) {
        if !self.is_terminal() {
            self.bytes_total = total;
        }
    }

    pub fn set_current(&mut self, path: impl Into<String>) {
        if !self.is_terminal() {
            self.current = path.into();
        }
    }

    pub fn add_bytes(&mut self, n: u64) {
        if !self.is_terminal() {
            self.bytes_done = self.bytes_done.saturating_add(n);
        }
    }

    pub fn finish(&mut self) {
        if !self.is_terminal() {
            self.state = SftpJobState::Done;
        }
    }

    pub fn fail(&mut self, reason: impl Into<String>) {
        if !self.is_terminal() {
            self.state = SftpJobState::Error;
            self.error = Some(reason.into());
        }
    }

    pub fn cancel(&mut self) {
        if !self.is_terminal() {
            self.state = SftpJobState::Cancelled;
        }
    }
}

impl Default for JobProgress {
    fn default() -> Self {
        Self::new()
    }
}

/// A live/finished transfer job. Progress lives behind a `Mutex` so the transfer
/// task updates it while the GUI polls it.
struct Job {
    id: u64,
    pane_id: u64,
    kind: SftpTransferKind,
    local: String,
    remote: String,
    cancel: AtomicBool,
    progress: Mutex<JobProgress>,
    done_at: Mutex<Option<Instant>>,
}

impl Job {
    fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    fn set_total(&self, total: u64) {
        self.progress.lock().unwrap().set_total(total);
    }

    fn set_current(&self, path: impl Into<String>) {
        self.progress.lock().unwrap().set_current(path);
    }

    fn add_bytes(&self, n: u64) {
        self.progress.lock().unwrap().add_bytes(n);
    }

    fn finish(&self) {
        self.progress.lock().unwrap().finish();
        *self.done_at.lock().unwrap() = Some(Instant::now());
    }

    fn fail(&self, reason: impl Into<String>) {
        self.progress.lock().unwrap().fail(reason);
        *self.done_at.lock().unwrap() = Some(Instant::now());
    }

    fn mark_cancelled(&self) {
        self.progress.lock().unwrap().cancel();
        *self.done_at.lock().unwrap() = Some(Instant::now());
    }

    fn snapshot(&self) -> SftpJobProgress {
        let p = self.progress.lock().unwrap();
        SftpJobProgress {
            job_id: self.id,
            pane_id: self.pane_id,
            kind: self.kind,
            state: p.state,
            current: p.current.clone(),
            bytes_done: p.bytes_done,
            bytes_total: p.bytes_total,
            error: p.error.clone(),
            local: self.local.clone(),
            remote: self.remote.clone(),
        }
    }

    /// True once terminal and past the retention window (safe to prune).
    fn is_expired(&self) -> bool {
        matches!(
            *self.done_at.lock().unwrap(),
            Some(t) if t.elapsed() > JOB_RETENTION
        )
    }
}

// ---------------------------------------------------------------------------
// The manager.
// ---------------------------------------------------------------------------

/// A per-connection SFTP-session cache slot. The inner `tokio::Mutex` serializes
/// opening (so two panes racing to first-use a connection open one session, not
/// two) without serializing *different* connections.
struct SessionSlot {
    inner: tokio::sync::Mutex<Option<CachedSession>>,
}

struct CachedSession {
    conn: Weak<SshConnection>,
    sftp: Arc<SftpSession>,
}

pub struct SftpManager {
    sessions: Mutex<HashMap<ConnectionKey, Arc<SessionSlot>>>,
    jobs: Mutex<HashMap<u64, Arc<Job>>>,
    next_job: AtomicU64,
}

impl SftpManager {
    /// The process-wide SFTP engine.
    pub fn global() -> &'static SftpManager {
        static MANAGER: OnceLock<SftpManager> = OnceLock::new();
        MANAGER.get_or_init(|| SftpManager {
            sessions: Mutex::new(HashMap::new()),
            jobs: Mutex::new(HashMap::new()),
            next_job: AtomicU64::new(1),
        })
    }

    // --- sync entry points (called from the server's std threads) ----------

    /// List a remote directory. Blocks the calling thread on the SSH runtime.
    pub fn list(&self, conn: &Arc<SshConnection>, path: &str) -> Result<Vec<SftpEntry>, String> {
        SshManager::global().handle().block_on(async {
            self.with_session(conn, |sftp| async move { list_dir(&sftp, path).await })
                .await
        })
    }

    /// Run a one-shot filesystem operation.
    pub fn op(&self, conn: &Arc<SshConnection>, op: &SftpOp) -> SftpOpResult {
        let result = SshManager::global().handle().block_on(async {
            self.with_session(conn, |sftp| async move { run_op(&sftp, op).await })
                .await
        });
        match result {
            Ok(r) => r,
            Err(e) => SftpOpResult::Error(e),
        }
    }

    /// Start a background transfer. Returns the new job id immediately; the
    /// transfer runs on the SSH runtime and reports progress via `list_jobs`.
    pub fn start_transfer(
        &'static self,
        conn: &Arc<SshConnection>,
        spec: SftpTransferSpec,
    ) -> Result<u64, String> {
        // Establish the session up-front so an immediate failure (no SFTP) is
        // reported synchronously rather than as a phantom job.
        let sftp = SshManager::global()
            .handle()
            .block_on(async { self.session_for(conn).await })?;

        let id = self.next_job.fetch_add(1, Ordering::Relaxed);
        let job = Arc::new(Job {
            id,
            pane_id: spec.pane_id,
            kind: spec.kind,
            local: spec.local.to_string_lossy().to_string(),
            remote: spec.remote.clone(),
            cancel: AtomicBool::new(false),
            progress: Mutex::new(JobProgress::new()),
            done_at: Mutex::new(None),
        });
        self.jobs.lock().unwrap().insert(id, job.clone());

        SshManager::global().handle().spawn(async move {
            run_transfer(sftp, spec, job).await;
        });
        Ok(id)
    }

    /// Cancel a running job (idempotent). Returns the current progress list for
    /// the job's pane so the caller can refresh the tray in one round-trip.
    pub fn cancel(&self, job_id: u64) -> Vec<SftpJobProgress> {
        let pane = {
            let jobs = self.jobs.lock().unwrap();
            if let Some(job) = jobs.get(&job_id) {
                job.cancel.store(true, Ordering::SeqCst);
                Some(job.pane_id)
            } else {
                None
            }
        };
        match pane {
            Some(pane_id) => self.list_jobs(pane_id),
            None => Vec::new(),
        }
    }

    /// Snapshot the transfer jobs for a pane, pruning expired (long-finished)
    /// ones as a side effect so the table stays bounded.
    pub fn list_jobs(&self, pane_id: u64) -> Vec<SftpJobProgress> {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.retain(|_, job| !job.is_expired());
        let mut out: Vec<SftpJobProgress> = jobs
            .values()
            .filter(|j| j.pane_id == pane_id)
            .map(|j| j.snapshot())
            .collect();
        out.sort_by_key(|j| j.job_id);
        out
    }

    // --- session cache -----------------------------------------------------

    /// Run `f` against the pane's cached SFTP session, retrying once with a
    /// freshly re-opened session **only** when the first attempt failed for a
    /// transport/channel reason (the cached subsystem channel died while the
    /// connection lives). A logical SFTP failure — a server status like permission
    /// denied or no-such-file — returns directly, never re-opening the session (a
    /// retry would just fail identically and waste a round-trip). See
    /// [`is_transport_failure`].
    async fn with_session<T, F, Fut>(&self, conn: &Arc<SshConnection>, f: F) -> Result<T, String>
    where
        F: Fn(Arc<SftpSession>) -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        let sftp = self.session_for(conn).await?;
        match f(sftp).await {
            Ok(v) => Ok(v),
            Err(e) if is_transport_failure(&e) => {
                self.invalidate(conn.key());
                let sftp = self.session_for(conn).await?;
                f(sftp).await
            }
            Err(e) => Err(e),
        }
    }

    /// The cached session for `conn`, opening one if absent or stale.
    async fn session_for(&self, conn: &Arc<SshConnection>) -> Result<Arc<SftpSession>, String> {
        let slot = {
            let mut map = self.sessions.lock().unwrap();
            map.entry(conn.key().clone())
                .or_insert_with(|| {
                    Arc::new(SessionSlot {
                        inner: tokio::sync::Mutex::new(None),
                    })
                })
                .clone()
        };
        let mut guard = slot.inner.lock().await;
        if let Some(cached) = guard.as_ref() {
            let same = cached.conn.upgrade().is_some_and(|c| Arc::ptr_eq(&c, conn));
            if same && conn.is_alive() {
                return Ok(cached.sftp.clone());
            }
        }
        let sftp = open_sftp(conn).await?;
        *guard = Some(CachedSession {
            conn: Arc::downgrade(conn),
            sftp: sftp.clone(),
        });
        Ok(sftp)
    }

    fn invalidate(&self, key: &ConnectionKey) {
        self.sessions.lock().unwrap().remove(key);
    }
}

/// Whether a stringified SFTP op error looks like a *transport/channel* failure
/// (the subsystem channel died) rather than a logical server status (permission
/// denied, no such file, …). Only the former is worth re-opening the session for.
///
/// `russh_sftp` renders channel/IO failures with these markers; a server status
/// code renders as `<code>: <message>` and matches none of them — so an unmatched
/// (logical) error is not retried. Conservative by design: an unrecognized error
/// is treated as logical and returned directly.
fn is_transport_failure(msg: &str) -> bool {
    const MARKERS: &[&str] = &[
        "I/O:",           // russh_sftp `Error::IO` — the channel stream failed
        "Unexpected EOF", // the stream closed mid-message
        "Timeout",        // no response — the subsystem/channel is wedged
        "Unexpected packet",
        "SendError", // the channel task's receiver is gone
        "RecvError", // the channel task ended before replying
    ];
    MARKERS.iter().any(|m| msg.contains(m))
}

/// Open a fresh SFTP subsystem channel on `conn` and hand back a session.
async fn open_sftp(conn: &Arc<SshConnection>) -> Result<Arc<SftpSession>, String> {
    let channel = conn
        .open_session_channel()
        .await
        .map_err(|e| format!("open sftp channel failed: {e}"))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| format!("sftp subsystem request failed: {e}"))?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| format!("sftp init failed: {e}"))?;
    Ok(Arc::new(sftp))
}

// ---------------------------------------------------------------------------
// Operations.
// ---------------------------------------------------------------------------

async fn list_dir(sftp: &SftpSession, path: &str) -> Result<Vec<SftpEntry>, String> {
    let read_dir = sftp.read_dir(path).await.map_err(|e| format!("{e}"))?;
    let mut out = Vec::new();
    for entry in read_dir {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let attrs = entry.metadata();
        let mut e = entry_from_attrs(&name, &attrs);
        if e.kind == SftpEntryKind::Symlink {
            // Follow-stat the target so the GUI knows navigate-vs-download.
            if let Ok(target) = sftp.metadata(remote_join(path, &name)).await {
                e.target_is_dir = target.is_dir();
            }
        }
        out.push(e);
    }
    Ok(out)
}

async fn run_op(sftp: &SftpSession, op: &SftpOp) -> Result<SftpOpResult, String> {
    Ok(match op {
        SftpOp::Stat { path } => {
            let attrs = sftp
                .metadata(path.clone())
                .await
                .map_err(|e| format!("{e}"))?;
            SftpOpResult::Stat(entry_from_attrs(&remote_basename(path), &attrs))
        }
        SftpOp::Mkdir { path } => {
            sftp.create_dir(path.clone())
                .await
                .map_err(|e| format!("{e}"))?;
            SftpOpResult::Done
        }
        SftpOp::CreateFile { path } => {
            // EXCLUDE => fail rather than clobber an existing file. The OPEN
            // itself creates the (empty) file server-side; flush/shutdown closes
            // the handle cleanly.
            let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE;
            let mut file = sftp
                .open_with_flags(path.clone(), flags)
                .await
                .map_err(|e| format!("{e}"))?;
            file.flush().await.ok();
            file.shutdown().await.ok();
            SftpOpResult::Done
        }
        SftpOp::RemoveFile { path } => {
            sftp.remove_file(path.clone())
                .await
                .map_err(|e| format!("{e}"))?;
            SftpOpResult::Done
        }
        SftpOp::RemoveDir { path } => {
            remove_dir_recursive(sftp, path).await?;
            SftpOpResult::Done
        }
        SftpOp::Rename { from, to } => {
            // Plain rename, no overwrite: a user rename onto an existing name
            // must fail, not silently delete the target (`rename_over` is for
            // the upload temp-swap only, where we own both paths).
            sftp.rename(from.clone(), to.clone())
                .await
                .map_err(|e| format!("rename failed: {e}"))?;
            SftpOpResult::Done
        }
        SftpOp::Chmod { path, mode } => {
            let mut attrs = FileAttributes::empty();
            attrs.permissions = Some(*mode);
            sftp.set_metadata(path.clone(), attrs)
                .await
                .map_err(|e| format!("{e}"))?;
            SftpOpResult::Done
        }
        SftpOp::Readlink { path } => {
            let target = sftp
                .read_link(path.clone())
                .await
                .map_err(|e| format!("{e}"))?;
            SftpOpResult::Link(target)
        }
    })
}

/// Rename `from` over `to`, tolerating a server that refuses to overwrite an
/// existing target: remove the target first, then retry. (See the module note on
/// posix-rename.)
async fn rename_over(sftp: &SftpSession, from: &str, to: &str) -> Result<(), String> {
    if sftp.rename(from.to_string(), to.to_string()).await.is_ok() {
        return Ok(());
    }
    let _ = sftp.remove_file(to.to_string()).await;
    sftp.rename(from.to_string(), to.to_string())
        .await
        .map_err(|e| format!("rename failed: {e}"))
}

/// Daemon-side recursive directory delete: remove children (files and links
/// directly; subdirectories by recursion) then the directory itself. A
/// symlink child is unlinked, never followed.
async fn remove_dir_recursive(sftp: &SftpSession, path: &str) -> Result<(), String> {
    // Explicit worklist to avoid async recursion. Each dir is visited twice:
    // first to enqueue its children, then (after them) to remove the now-empty
    // directory. We push a directory's own removal marker before its children so
    // that, popping LIFO, children are removed first.
    enum Step {
        Enter(String),
        RemoveDir(String),
    }
    let mut stack = vec![Step::Enter(path.to_string())];
    while let Some(step) = stack.pop() {
        match step {
            Step::Enter(dir) => {
                stack.push(Step::RemoveDir(dir.clone()));
                let read_dir = sftp
                    .read_dir(dir.clone())
                    .await
                    .map_err(|e| format!("{e}"))?;
                for entry in read_dir {
                    let name = entry.file_name();
                    if name == "." || name == ".." {
                        continue;
                    }
                    let child = remote_join(&dir, &name);
                    let attrs = entry.metadata();
                    // Only a real directory recurses; a symlink (even to a dir) is
                    // unlinked as a file so we never delete through it.
                    if attrs.is_dir() && !attrs.is_symlink() {
                        stack.push(Step::Enter(child));
                    } else {
                        // Best-effort: a child already gone is fine.
                        let _ = sftp.remove_file(child).await;
                    }
                }
            }
            Step::RemoveDir(dir) => {
                sftp.remove_dir(dir).await.map_err(|e| format!("{e}"))?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Transfers.
// ---------------------------------------------------------------------------

async fn run_transfer(sftp: Arc<SftpSession>, spec: SftpTransferSpec, job: Arc<Job>) {
    let result = match spec.kind {
        SftpTransferKind::Download => download(&sftp, &spec, &job).await,
        SftpTransferKind::Upload => upload(&sftp, &spec, &job).await,
    };
    match result {
        Ok(()) => job.finish(),
        Err(_) if job.is_cancelled() => job.mark_cancelled(),
        Err(e) => job.fail(e),
    }
}

/// A cancelled job surfaces as an `Err` that `run_transfer` maps to `Cancelled`.
fn cancelled() -> String {
    "cancelled".to_string()
}

async fn download(sftp: &SftpSession, spec: &SftpTransferSpec, job: &Job) -> Result<(), String> {
    // Size pre-pass (recursive) so the tray has a denominator.
    let total = remote_size(sftp, &spec.remote, spec.recursive, job).await?;
    job.set_total(total);

    // The root is stat'ed (following a symlink deliberately — the user picked
    // it); children carry their lstat-style attrs from the directory listing so
    // symlinks are recognized and skipped, never followed: following them would
    // loop forever on a cyclic link and copy whole trees through e.g. `-> /`.
    let root_attrs = sftp
        .metadata(spec.remote.clone())
        .await
        .map_err(|e| format!("{e}"))?;
    let mut stack = vec![(spec.remote.clone(), spec.local.clone(), root_attrs)];
    while let Some((rpath, lpath, attrs)) = stack.pop() {
        if job.is_cancelled() {
            return Err(cancelled());
        }
        if attrs.is_dir() {
            if !spec.recursive {
                return Err("remote path is a directory (enable recursive)".to_string());
            }
            tokio::fs::create_dir_all(&lpath)
                .await
                .map_err(|e| format!("create local dir: {e}"))?;
            let read_dir = sftp
                .read_dir(rpath.clone())
                .await
                .map_err(|e| format!("{e}"))?;
            for entry in read_dir {
                let name = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                // Guard against a hostile server returning a traversing name
                // (`..`, `a/b`, `/abs`): it would become a local path component
                // via `lpath.join`, escaping the destination. Skip unsafe names.
                if !safe_local_name(&name) {
                    log::warn!(
                        "sftp download: skipping remote entry with unsafe name {name:?} under {rpath}"
                    );
                    continue;
                }
                let cattrs = entry.metadata();
                if cattrs.is_symlink() {
                    log::debug!("sftp download: skipping symlink {name:?} under {rpath}");
                    continue;
                }
                stack.push((remote_join(&rpath, &name), lpath.join(&name), cattrs));
            }
        } else {
            download_file(sftp, &rpath, &lpath, attrs.permissions, job).await?;
        }
    }
    Ok(())
}

async fn download_file(
    sftp: &SftpSession,
    rpath: &str,
    lpath: &Path,
    mode: Option<u32>,
    job: &Job,
) -> Result<(), String> {
    job.set_current(rpath.to_string());
    if let Some(parent) = lpath.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    // Download to a per-file temp in the destination dir, then rename over the
    // target on success — mirroring the upload temp+rename discipline so a failed
    // or cancelled download never truncates a pre-existing local file in place.
    let temp = download_temp_path(lpath);
    let result: Result<(), String> = async {
        let mut remote = sftp
            .open(rpath.to_string())
            .await
            .map_err(|e| format!("{e}"))?;
        let mut local = tokio::fs::File::create(&temp)
            .await
            .map_err(|e| format!("create {}: {e}", temp.display()))?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            if job.is_cancelled() {
                return Err(cancelled());
            }
            let n = remote.read(&mut buf).await.map_err(|e| format!("{e}"))?;
            if n == 0 {
                break;
            }
            local
                .write_all(&buf[..n])
                .await
                .map_err(|e| format!("write local: {e}"))?;
            job.add_bytes(n as u64);
        }
        // A failed flush means the temp is incomplete (e.g. disk full) — it must
        // abort here, before the rename commits the temp over a good target.
        local
            .flush()
            .await
            .map_err(|e| format!("write local: {e}"))?;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        // Best effort: drop the partial temp, leaving any pre-existing target intact.
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(e);
    }
    // Swap the completed temp over the target.
    if let Err(e) = tokio::fs::rename(&temp, lpath).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(format!("rename into {}: {e}", lpath.display()));
    }
    // Preserve the executable/permission bits where sane (unix only, low 12 bits).
    preserve_mode(lpath, mode);
    Ok(())
}

async fn upload(sftp: &SftpSession, spec: &SftpTransferSpec, job: &Job) -> Result<(), String> {
    let total = local_size(&spec.local, spec.recursive, job).await?;
    job.set_total(total);

    // Mirrors the download walker's symlink policy: the root is stat'ed
    // (following a symlink deliberately), children are classified by their
    // lstat-style file type and symlinks are skipped, never followed.
    let root_is_dir = tokio::fs::metadata(&spec.local)
        .await
        .map_err(|e| format!("stat {}: {e}", spec.local.display()))?
        .is_dir();
    let mut stack = vec![(spec.local.clone(), spec.remote.clone(), root_is_dir)];
    while let Some((lpath, rpath, is_dir)) = stack.pop() {
        if job.is_cancelled() {
            return Err(cancelled());
        }
        if is_dir {
            if !spec.recursive {
                return Err("local path is a directory (enable recursive)".to_string());
            }
            // Create the remote dir (ignore "already exists").
            let _ = sftp.create_dir(rpath.clone()).await;
            let mut read_dir = tokio::fs::read_dir(&lpath)
                .await
                .map_err(|e| format!("read local dir: {e}"))?;
            while let Some(child) = read_dir
                .next_entry()
                .await
                .map_err(|e| format!("read local dir: {e}"))?
            {
                let ftype = child
                    .file_type()
                    .await
                    .map_err(|e| format!("stat {}: {e}", child.path().display()))?;
                if ftype.is_symlink() {
                    log::debug!("sftp upload: skipping symlink {}", child.path().display());
                    continue;
                }
                let name = child.file_name().to_string_lossy().to_string();
                stack.push((child.path(), remote_join(&rpath, &name), ftype.is_dir()));
            }
        } else {
            upload_file(sftp, &lpath, &rpath, job).await?;
        }
    }
    Ok(())
}

async fn upload_file(
    sftp: &SftpSession,
    lpath: &Path,
    rpath: &str,
    job: &Job,
) -> Result<(), String> {
    job.set_current(rpath.to_string());
    let temp = upload_temp_name(rpath);
    let mut local = tokio::fs::File::open(lpath)
        .await
        .map_err(|e| format!("open {}: {e}", lpath.display()))?;
    let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
    let mut remote = match sftp.open_with_flags(temp.clone(), flags).await {
        Ok(f) => f,
        Err(e) => return Err(format!("open remote temp: {e}")),
    };
    let mut buf = vec![0u8; CHUNK];
    let result: Result<(), String> = async {
        loop {
            if job.is_cancelled() {
                return Err(cancelled());
            }
            let n = local.read(&mut buf).await.map_err(|e| format!("{e}"))?;
            if n == 0 {
                break;
            }
            remote
                .write_all(&buf[..n])
                .await
                .map_err(|e| format!("write remote: {e}"))?;
            job.add_bytes(n as u64);
        }
        // Surface late write errors before the rename commits the temp over the
        // target; a truncated temp must fail the transfer, not replace the file.
        remote
            .flush()
            .await
            .map_err(|e| format!("write remote: {e}"))?;
        remote
            .shutdown()
            .await
            .map_err(|e| format!("write remote: {e}"))?;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        // Clean up the partial temp file, best effort.
        let _ = sftp.remove_file(temp.clone()).await;
        return Err(e);
    }
    // Swap the temp over the target.
    if let Err(e) = rename_over(sftp, &temp, rpath).await {
        let _ = sftp.remove_file(temp).await;
        return Err(e);
    }
    Ok(())
}

/// Recursively sum remote file sizes (files only). Cancellation short-circuits.
async fn remote_size(
    sftp: &SftpSession,
    root: &str,
    recursive: bool,
    job: &Job,
) -> Result<u64, String> {
    let mut total = 0u64;
    let root_attrs = match sftp.metadata(root.to_string()).await {
        Ok(a) => a,
        Err(_) => return Ok(0),
    };
    let mut stack = vec![(root.to_string(), root_attrs)];
    while let Some((path, attrs)) = stack.pop() {
        if job.is_cancelled() {
            return Err(cancelled());
        }
        if attrs.is_dir() {
            if !recursive {
                continue;
            }
            if let Ok(read_dir) = sftp.read_dir(path.clone()).await {
                for entry in read_dir {
                    let name = entry.file_name();
                    if name == "." || name == ".." {
                        continue;
                    }
                    // Skip the same unsafe names and symlinks the download walker
                    // skips so the size denominator matches what is transferred.
                    if !safe_local_name(&name) {
                        continue;
                    }
                    let cattrs = entry.metadata();
                    if cattrs.is_symlink() {
                        continue;
                    }
                    stack.push((remote_join(&path, &name), cattrs));
                }
            }
        } else {
            total = total.saturating_add(attrs.size.unwrap_or(0));
        }
    }
    Ok(total)
}

/// Recursively sum local file sizes (files only).
async fn local_size(root: &Path, recursive: bool, job: &Job) -> Result<u64, String> {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if job.is_cancelled() {
            return Err(cancelled());
        }
        // Root uses stat (a root symlink is followed deliberately); children
        // below use their lstat file type, so links are counted at zero and
        // never followed — matching the upload walker.
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            if !recursive {
                continue;
            }
            if let Ok(mut rd) = tokio::fs::read_dir(&path).await {
                while let Ok(Some(child)) = rd.next_entry().await {
                    match child.file_type().await {
                        Ok(t) if t.is_symlink() => continue,
                        Ok(_) => stack.push(child.path()),
                        Err(_) => continue,
                    }
                }
            }
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    Ok(total)
}

/// Apply the sane low permission bits of a downloaded file locally (unix only).
#[cfg(unix)]
fn preserve_mode(path: &Path, mode: Option<u32>) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        let bits = mode & 0o777;
        if bits != 0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits));
        }
    }
}

#[cfg(not(unix))]
fn preserve_mode(_path: &Path, _mode: Option<u32>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_join_handles_root_and_nested_and_slashes() {
        assert_eq!(remote_join("/", "file"), "/file");
        assert_eq!(remote_join("", "file"), "/file");
        assert_eq!(remote_join("/home/deploy", "src"), "/home/deploy/src");
        // Trailing/leading slashes are normalized to a single separator.
        assert_eq!(remote_join("/home/deploy/", "/src"), "/home/deploy/src");
        // Unicode names survive intact.
        assert_eq!(remote_join("/家", "文件"), "/家/文件");
    }

    #[test]
    fn remote_parent_walks_up_and_stops_at_root() {
        assert_eq!(remote_parent("/home/deploy/src"), "/home/deploy");
        assert_eq!(remote_parent("/home"), "/");
        assert_eq!(remote_parent("/"), "/");
        assert_eq!(remote_parent(""), "/");
        // Trailing slash ignored.
        assert_eq!(remote_parent("/a/b/"), "/a");
        assert_eq!(remote_parent("/项目/子"), "/项目");
    }

    #[test]
    fn remote_basename_extracts_final_component() {
        assert_eq!(remote_basename("/a/b/c"), "c");
        assert_eq!(remote_basename("/a/b/"), "b");
        assert_eq!(remote_basename("/"), "/");
        assert_eq!(remote_basename("/项目/子"), "子");
    }

    #[test]
    fn upload_temp_name_is_distinct_and_marked() {
        let a = upload_temp_name("/dir/file.txt");
        let b = upload_temp_name("/dir/file.txt");
        assert!(a.starts_with("/dir/file.txt.tty7-upload-"));
        assert!(b.starts_with("/dir/file.txt.tty7-upload-"));
        // Two temp names for the same target must differ (counter component).
        assert_ne!(a, b);
    }

    #[test]
    fn safe_local_name_rejects_traversal_and_accepts_plain_names() {
        // Rejected: empty, dot, dotdot, embedded/leading separators, absolute.
        assert!(!safe_local_name(""));
        assert!(!safe_local_name("."));
        assert!(!safe_local_name(".."));
        assert!(!safe_local_name("a/b"));
        assert!(!safe_local_name("/abs"));
        assert!(!safe_local_name("../../.ssh/authorized_keys"));
        assert!(!safe_local_name("a\\b"));
        // Accepted: ordinary single components, including Unicode and dotted names.
        assert!(safe_local_name("file.txt"));
        assert!(safe_local_name("项目"));
        assert!(safe_local_name("a.tar.gz"));
        assert!(safe_local_name(".hidden"));
    }

    #[test]
    fn download_temp_path_is_sibling_and_distinct() {
        let target = Path::new("/dest/dir/file.bin");
        let a = download_temp_path(target);
        let b = download_temp_path(target);
        // Same directory as the target (so the finishing rename is same-filesystem).
        assert_eq!(a.parent(), target.parent());
        assert!(
            a.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("file.bin.tty7-download-")
        );
        // Two temps for the same target differ (counter component).
        assert_ne!(a, b);
    }

    #[test]
    fn is_transport_failure_distinguishes_channel_from_logical_errors() {
        // Transport/channel failures → retry.
        assert!(is_transport_failure("I/O: broken pipe"));
        assert!(is_transport_failure("rename failed: I/O: connection reset"));
        assert!(is_transport_failure("Unexpected EOF on stream"));
        assert!(is_transport_failure("Timeout"));
        assert!(is_transport_failure("SendError: channel closed"));
        // Logical server statuses → no retry.
        assert!(!is_transport_failure("3: Permission denied"));
        assert!(!is_transport_failure("2: No such file or directory"));
        assert!(!is_transport_failure(
            "remote path is a directory (enable recursive)"
        ));
    }

    #[test]
    fn classify_prefers_symlink_over_regular_bit() {
        // S_IFLNK carries the S_IFREG bit too; symlink must win.
        let mut link = FileAttributes::empty();
        link.permissions = Some(0o120777);
        assert_eq!(classify(&link), SftpEntryKind::Symlink);

        let mut dir = FileAttributes::empty();
        dir.permissions = Some(0o040755);
        assert_eq!(classify(&dir), SftpEntryKind::Dir);

        let mut file = FileAttributes::empty();
        file.permissions = Some(0o100644);
        assert_eq!(classify(&file), SftpEntryKind::File);

        // Unknown permissions default to file.
        assert_eq!(classify(&FileAttributes::empty()), SftpEntryKind::File);
    }

    #[test]
    fn entry_from_attrs_maps_fields() {
        let mut attrs = FileAttributes::empty();
        attrs.size = Some(4096);
        attrs.mtime = Some(1_700_000_000);
        attrs.permissions = Some(0o100644);
        let e = entry_from_attrs("readme", &attrs);
        assert_eq!(e.name, "readme");
        assert_eq!(e.kind, SftpEntryKind::File);
        assert_eq!(e.size, 4096);
        assert_eq!(e.mtime, 1_700_000_000);
        assert_eq!(e.permissions, 0o100644);
        assert!(!e.target_is_dir);
    }

    #[test]
    fn job_progress_transitions_are_monotonic_and_latch() {
        let mut p = JobProgress::new();
        assert_eq!(p.state, SftpJobState::Running);

        p.set_total(1000);
        p.set_current("a.bin");
        p.add_bytes(400);
        p.add_bytes(200);
        assert_eq!(p.bytes_total, 1000);
        assert_eq!(p.bytes_done, 600);
        assert_eq!(p.current, "a.bin");

        p.finish();
        assert_eq!(p.state, SftpJobState::Done);

        // Terminal state latches: later transitions are ignored.
        p.add_bytes(999);
        p.fail("late error");
        p.cancel();
        assert_eq!(p.state, SftpJobState::Done);
        assert_eq!(p.bytes_done, 600);
        assert_eq!(p.error, None);
    }

    #[test]
    fn job_progress_cancel_and_fail_paths_latch() {
        let mut c = JobProgress::new();
        c.cancel();
        assert_eq!(c.state, SftpJobState::Cancelled);
        c.finish();
        assert_eq!(c.state, SftpJobState::Cancelled);

        let mut f = JobProgress::new();
        f.fail("boom");
        assert_eq!(f.state, SftpJobState::Error);
        assert_eq!(f.error.as_deref(), Some("boom"));
        f.finish();
        assert_eq!(f.state, SftpJobState::Error);
    }

    #[test]
    fn job_snapshot_reflects_progress() {
        let job = Job {
            id: 7,
            pane_id: 3,
            kind: SftpTransferKind::Download,
            local: "/l".into(),
            remote: "/r".into(),
            cancel: AtomicBool::new(false),
            progress: Mutex::new(JobProgress::new()),
            done_at: Mutex::new(None),
        };
        job.set_total(500);
        job.set_current("f");
        job.add_bytes(120);
        let snap = job.snapshot();
        assert_eq!(snap.job_id, 7);
        assert_eq!(snap.pane_id, 3);
        assert_eq!(snap.kind, SftpTransferKind::Download);
        assert_eq!(snap.state, SftpJobState::Running);
        assert_eq!(snap.bytes_total, 500);
        assert_eq!(snap.bytes_done, 120);
        assert_eq!(snap.current, "f");

        job.finish();
        assert_eq!(job.snapshot().state, SftpJobState::Done);
        assert!(!job.is_expired(), "just-finished job is not yet expired");
    }
}

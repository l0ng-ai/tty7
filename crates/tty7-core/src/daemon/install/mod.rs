//! Installing, launching and version-matching `tty7-server` on a remote machine.
//!
//! The six steps, in order:
//!
//! | | Step | Where |
//! |---|---|---|
//! | 1 | `uname -sm` → the release asset that runs there | [`asset::asset_for_uname`] |
//! | 2 | SFTP-stat `~/.local/share/tty7/bin/tty7-server-<client version>` | [`Installer::run`] |
//! | 3 | absent → download the asset **on the client** + sha256-verify it | [`download`], [`checksums`] |
//! | 4 | SFTP-put into `bin/.tty7-server-<ver>.tmp` | [`RemoteOps::put`] |
//! | 5 | `chmod 0755` then `rename` — atomic publish | [`RemoteOps::rename`] |
//! | 6 | probe the remote control socket; nothing there → launch a detached daemon | [`Installer::ensure_daemon`] |
//!
//! ## Why the client downloads
//!
//! Design D5: the client fetches the binary over HTTPS and pushes it over the
//! existing SSH connection, rather than having the remote `curl` it. Machines
//! behind a jump host or on an air-gapped internal network cannot reach GitHub —
//! and those are a large share of the machines this feature exists for. The
//! client always can, because it just downloaded its own copy of tty7 the same
//! way.
//!
//! ## …except for WSL, which downloads nothing
//!
//! A WSL distro is served the Linux binary the
//! *Windows client already shipped with*, not one fetched from a release. Both
//! paths meet at [`ServerBinarySource`] — [`ReleaseDownload`] for a real remote,
//! [`wsl::BundledServerBinary`] for a distro on this machine — so steps 2 and
//! 4-6 (stat, upload, atomic publish, launch) are literally the same code, and
//! only "where do the bytes come from" differs. See [`wsl`].
//!
//! ## What is injected, and why
//!
//! Three seams — [`RemoteOps`] (SSH/SFTP), [`AssetFetcher`] (HTTPS), and
//! [`InstallConfirm`] (the user) — so the whole flow can be driven by fakes. The
//! failure paths that matter most here (a sha256 mismatch, a full disk, a refused
//! consent) are exactly the ones you cannot conjure on a real machine on demand,
//! so they have to be reachable without one.
//!
//! ## Scope
//!
//! Nothing here uses `sudo` or writes outside `$HOME`. Nothing here opens
//! the workspace link either: this module's contract with the transport
//! (`remote_link` / the SSH router) is exactly [`ensure_remote_server`] — call it,
//! and on `Ok` the far end has the right binary installed and a daemon serving.

use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub mod asset;
pub mod checksums;
#[cfg(feature = "remote-install")]
pub mod download;
pub mod ssh_ops;
pub mod wsl;

pub use asset::{RemotePaths, UnsupportedTarget};
pub use checksums::ChecksumError;

use crate::daemon::ssh::SshConnection;

/// The client version, which is also the version of the server it installs.
/// Client and server ship from the same workspace version, so "the server that
/// matches me" is always `tty7-server-<this>`.
pub fn client_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Mode bits for the installed binary: owner-executable, world-readable. Not
/// 0700 — the *directory* is 0700, which is what actually scopes access, and a
/// 0755 binary matches what every other user-local install looks like.
const BINARY_MODE: u32 = 0o755;
/// Mode bits for every directory we create (directories 0700).
const DIR_MODE: u32 = 0o700;

/// How long a freshly launched remote daemon gets to start answering on its
/// control socket. Longer than the local [`crate::daemon::spawn`] budget: every
/// probe is an SSH round trip, and the far end may be a loaded or distant box.
const REMOTE_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
/// Gap between probes while waiting for a launched daemon.
const REMOTE_POLL_INTERVAL: Duration = Duration::from_millis(400);
/// How long a remote daemon asked to stop gets before we conclude it will not.
const REMOTE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Injection seams.
// ---------------------------------------------------------------------------

/// The result of running one command on the remote machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    /// The command's exit status, or `None` if the channel closed without one
    /// (killed by a signal, or a server that does not report status).
    pub status: Option<u32>,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    /// The most informative one-line reason a command failed, for error
    /// messages: stderr if it said anything, else the exit status.
    pub(crate) fn failure_reason(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            return stderr.lines().next().unwrap_or(stderr).to_string();
        }
        match self.status {
            Some(code) => format!("exit status {code}"),
            None => "the command was killed before it reported a status".to_string(),
        }
    }
}

/// What a remote path is, if it is anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteStat {
    pub size: u64,
    pub mode: u32,
    pub is_dir: bool,
}

/// Everything the installer needs to do *on the remote machine*: run a command
/// and manipulate files. Implemented for real over SSH + SFTP in [`ssh_ops`],
/// and by an in-memory fake in this module's tests.
///
/// Errors are strings because that is what the SFTP layer produces and because
/// every one of them is destined for a user-visible message that quotes the
/// server's own wording — "Failure" from a full disk, "Permission denied" from a
/// read-only home. Classification back into structure happens in
/// [`InstallError`], where the path is known too.
pub trait RemoteOps: Send + Sync {
    /// The remote user's home directory, absolute. SFTP has no `~` expansion, so
    /// every path the installer builds starts here.
    fn home_dir(&self) -> Result<String, String>;
    /// Run `cmd` through the remote's shell and collect its output.
    fn run(&self, cmd: &str) -> Result<ExecOutput, String>;
    /// Start `cmd` and return as soon as it has been accepted, without waiting
    /// for it to exit. Used only for the daemon launch, which by design never
    /// exits.
    fn spawn_detached(&self, cmd: &str) -> Result<(), String>;
    /// A wait to run in the *same* invocation as the daemon launch, right after
    /// the launch line, for a transport that cannot let the invocation return
    /// while the daemon is still fragile. `binary` is the server just launched.
    ///
    /// Defaulted to nothing, because SSH needs nothing: closing an exec channel
    /// is unhurried and a backgrounded daemon survives it. WSL is the one
    /// transport that does — see [`wsl::launch_settle`](super::install::wsl).
    fn launch_settle(&self, _binary: &str) -> Option<String> {
        None
    }
    /// `stat` following symlinks. `Ok(None)` means "not there", which is a normal
    /// answer, not an error.
    fn stat(&self, path: &str) -> Result<Option<RemoteStat>, String>;
    /// Create one directory. Succeeding when it already exists is the
    /// implementation's job (SFTP servers disagree about which status they
    /// return for that).
    fn mkdir(&self, path: &str) -> Result<(), String>;
    fn chmod(&self, path: &str, mode: u32) -> Result<(), String>;
    /// Write `bytes` to `path`, truncating anything already there.
    fn put(&self, path: &str, bytes: &[u8]) -> Result<(), String>;
    /// [`put`](Self::put), calling `on_progress(written)` as the write
    /// advances. Defaulted to plain `put` for the same reason as
    /// [`AssetFetcher::get_with_progress`]: an in-memory fake writes all of it
    /// at once and has no intermediate state to report.
    fn put_with_progress(
        &self,
        path: &str,
        bytes: &[u8],
        on_progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<(), String> {
        let result = self.put(path, bytes);
        if result.is_ok() {
            on_progress(bytes.len() as u64);
        }
        result
    }
    /// Rename `from` over `to`. Same directory, so same filesystem, so atomic.
    fn rename(&self, from: &str, to: &str) -> Result<(), String>;
    fn remove_file(&self, path: &str) -> Result<(), String>;
    /// Entry names (not paths) in a directory. `Ok(None)` if it does not exist.
    fn list_dir(&self, path: &str) -> Result<Option<Vec<String>>, String>;
}

/// Fetches a release asset over HTTPS. The seam exists so the whole install
/// flow is testable without a network, and so the HTTP client stays behind one
/// small interface that `tty7-server` never links (see the `remote-install`
/// feature).
pub trait AssetFetcher: Send + Sync {
    fn get(&self, url: &str) -> Result<Vec<u8>, String>;

    /// [`get`](Self::get), calling `on_progress(done, total)` as the body
    /// arrives. `total` is the `Content-Length` when the server sent one.
    ///
    /// Defaulted to plain `get` so a fetcher that has nothing useful to say
    /// mid-transfer — every fake in the tests, and the checksums fetch, which is
    /// under a kilobyte — implements one method, not two. The real
    /// [`HttpsFetcher`](download::HttpsFetcher) overrides it.
    fn get_with_progress(
        &self,
        url: &str,
        on_progress: &dyn Fn(u64, Option<u64>),
    ) -> Result<Vec<u8>, String> {
        let _ = on_progress;
        self.get(url)
    }
}

/// A verified server binary, and where it came from.
pub struct LoadedBinary {
    pub bytes: Vec<u8>,
    /// Human-readable provenance, quoted verbatim in the consent prompt: a
    /// release URL for a downloaded asset, an absolute local path for a bundled
    /// one. The user is being asked to approve a write, and "from where" is half
    /// of what makes that question answerable.
    pub origin: String,
}

/// Length and provenance, never the bytes: a derived `Debug` here would put six
/// megabytes of ELF into a log line or a test failure.
impl std::fmt::Debug for LoadedBinary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedBinary")
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .field("origin", &self.origin)
            .finish()
    }
}

/// Where the bytes of a server binary come from.
///
/// Three implementations:
///
/// | Source | Used for | Integrity |
/// |---|---|---|
/// | [`ReleaseDownload`] | Any real remote | sha256 against the release's `checksums.txt` |
/// | [`wsl::BundledServerBinary`] | A WSL distro on this machine | The bytes shipped inside this install; whatever verified *this* client covers them |
/// | [`BundledOrRelease`] | A real remote when a local binary is on hand | The local copy if there is one, else the release's sha256 |
///
/// A WSL distro downloading its own copy would be absurd — the client is on the
/// same disk, often on a machine that reaches GitHub only through the proxy the
/// user is trying to escape — and a checksum manifest for a file we shipped
/// ourselves would verify nothing that the client's own signature did not.
pub trait ServerBinarySource: Send + Sync {
    fn load(&self, version: &str, asset: &'static str) -> Result<LoadedBinary, InstallError>;

    /// [`load`](Self::load), reporting bytes as they arrive.
    ///
    /// Defaulted to plain `load` because only one of the three sources has a
    /// transfer worth watching: [`wsl::BundledServerBinary`] reads a local file
    /// and is done before a bar could paint.
    fn load_with_progress(
        &self,
        version: &str,
        asset: &'static str,
        on_progress: &dyn Fn(u64, Option<u64>),
    ) -> Result<LoadedBinary, InstallError> {
        let _ = on_progress;
        self.load(version, asset)
    }
}

/// A local binary if [`wsl::BUNDLED_DIR_ENV`] names a directory holding one,
/// otherwise the release download.
///
/// Design D5 chose "client downloads, client uploads" because the *remote* is
/// often walled off from GitHub. But the client can be walled off too — an
/// air-gapped laptop, a TLS-intercepting corporate proxy (`ureq` trusts webpki
/// roots, not the system store), or simply a build with no published release,
/// which is every developer build and every `cargo install` from source. In all
/// of those the bytes are already on the disk and the download is the only thing
/// standing in the way.
///
/// **Opt-in, and never silent.** With the variable unset this is byte-for-byte
/// `ReleaseDownload`. Pointing it somewhere means "I vouch for these bytes":
/// there is no `checksums.txt` to verify a file the user placed by hand
/// against, exactly as with the WSL bundle. The install prompt still names the
/// origin, so the choice is visible at the moment it matters.
pub struct BundledOrRelease<'a> {
    pub fetch: &'a dyn AssetFetcher,
    /// Resolved once, at construction, rather than read from the environment
    /// per call — so the choice is a value the tests can hand in, and a
    /// mid-install change to the variable cannot make two steps disagree.
    pub bundled: Option<wsl::BundledServerBinary>,
}

impl<'a> BundledOrRelease<'a> {
    pub fn from_env(fetch: &'a dyn AssetFetcher) -> Self {
        Self {
            fetch,
            bundled: wsl::BundledServerBinary::from_env_only(),
        }
    }
}

impl ServerBinarySource for BundledOrRelease<'_> {
    fn load(&self, version: &str, asset: &'static str) -> Result<LoadedBinary, InstallError> {
        self.load_with_progress(version, asset, &|_, _| {})
    }

    fn load_with_progress(
        &self,
        version: &str,
        asset: &'static str,
        on_progress: &dyn Fn(u64, Option<u64>),
    ) -> Result<LoadedBinary, InstallError> {
        match &self.bundled {
            // A named directory that does *not* hold this asset is an error, not
            // a reason to fall back: someone who set the variable meant to
            // install from it, and quietly downloading instead would defeat
            // whichever of the reasons above they set it for.
            Some(bundled) => bundled.load(version, asset),
            None => ReleaseDownload { fetch: self.fetch }.load_with_progress(
                version,
                asset,
                on_progress,
            ),
        }
    }
}

/// The default source: fetch the release asset and its `checksums.txt` over
/// HTTPS, and verify one against the other before anything is written or the
/// user is asked.
pub struct ReleaseDownload<'a> {
    pub fetch: &'a dyn AssetFetcher,
}

impl ServerBinarySource for ReleaseDownload<'_> {
    fn load(&self, version: &str, asset: &'static str) -> Result<LoadedBinary, InstallError> {
        self.load_with_progress(version, asset, &|_, _| {})
    }

    fn load_with_progress(
        &self,
        version: &str,
        asset: &'static str,
        on_progress: &dyn Fn(u64, Option<u64>),
    ) -> Result<LoadedBinary, InstallError> {
        let tag = asset::release_tag(version);
        let manifest_url = asset::download_url(&tag, asset::CHECKSUMS_ASSET);
        // Not reported: `checksums.txt` is under a kilobyte, and a bar that
        // jumped to 100% for it before restarting for the real asset would read
        // as a stall rather than as two files.
        let manifest = self
            .fetch
            .get(&manifest_url)
            .map_err(|reason| InstallError::Download {
                url: manifest_url.clone(),
                reason,
            })?;
        let manifest = String::from_utf8(manifest).map_err(|_| InstallError::Download {
            url: manifest_url.clone(),
            reason: "checksums.txt is not valid UTF-8".to_string(),
        })?;

        let asset_url = asset::download_url(&tag, asset);
        let bytes = self
            .fetch
            .get_with_progress(&asset_url, on_progress)
            .map_err(|reason| InstallError::Download {
                url: asset_url.clone(),
                reason,
            })?;

        checksums::verify(&manifest, asset, &bytes).map_err(InstallError::Checksum)?;
        Ok(LoadedBinary {
            bytes,
            origin: asset_url,
        })
    }
}

// ---------------------------------------------------------------------------
// Consent — the decision point M5's UI plugs into.
// ---------------------------------------------------------------------------

/// Everything the user needs to answer "may tty7 write a binary onto this
/// machine?": what, where, how big, and where it came from.
///
/// `size_bytes` is the size of the bytes *already downloaded and verified*, not
/// a `Content-Length` guess — by the time this is raised the download has
/// happened and its sha256 matched, so the number quoted is exactly what will be
/// written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    /// A human label for the machine (`me@build-box:22`), for the prompt title.
    pub host: String,
    /// The version about to be installed (= the client's own version).
    pub version: String,
    /// The release asset name, e.g. `tty7-server-x86_64-unknown-linux-musl`.
    pub asset: &'static str,
    /// The URL it was downloaded from.
    pub source_url: String,
    /// Absolute remote path it will be published at.
    pub remote_path: String,
    /// Exact byte count that will be written.
    pub size_bytes: u64,
    /// Lowercase hex sha256 of those bytes, as published in `checksums.txt` and
    /// as verified locally. Shown so a cautious user can check it by hand.
    pub sha256: String,
}

/// The user's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallDecision {
    Approve,
    Decline,
}

/// Asks the user whether to write a server binary onto a machine for the first
/// time ("往别人机器上写二进制值得问一次").
///
/// **Only the first install on a given machine asks.** "First" is decided from
/// evidence on the remote itself — an empty (or absent)
/// `~/.local/share/tty7/bin` — rather than from client-side bookkeeping, so a
/// reinstalled client, a second laptop, or a wiped config dir does not re-ask
/// about a machine tty7 has demonstrably already written to. Later version
/// upgrades on that machine are silent, which is the design's explicit intent.
///
/// The default implementation ([`DenyInstall`]) **declines**. A headless daemon
/// with no UI attached must not decide on the user's behalf that writing to
/// their servers is fine; it fails with a message that says consent was never
/// obtained. M5's GUI registers a real one with [`set_install_confirm`].
pub trait InstallConfirm: Send + Sync {
    fn confirm(&self, request: &InstallRequest) -> InstallDecision;
}

/// The default: no UI, no consent, no install.
pub struct DenyInstall;

impl InstallConfirm for DenyInstall {
    fn confirm(&self, _request: &InstallRequest) -> InstallDecision {
        InstallDecision::Decline
    }
}

static CONFIRM: OnceLock<Mutex<Arc<dyn InstallConfirm>>> = OnceLock::new();

fn confirm_slot() -> &'static Mutex<Arc<dyn InstallConfirm>> {
    CONFIRM.get_or_init(|| Mutex::new(Arc::new(DenyInstall)))
}

/// Register the confirmation handler. Called once by the GUI at startup; last
/// call wins so a test can install its own.
pub fn set_install_confirm(confirm: Arc<dyn InstallConfirm>) {
    if let Ok(mut slot) = confirm_slot().lock() {
        *slot = confirm;
    }
}

thread_local! {
    /// A confirmation handler that outranks [`CONFIRM`] for the duration of one
    /// call, on one thread. See [`with_install_confirm`].
    static SCOPED_CONFIRM: std::cell::RefCell<Option<Arc<dyn InstallConfirm>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with `confirm` answering any install prompt it raises, then put the
/// previous handler back.
///
/// **Why a thread-local and not just [`set_install_confirm`].** The process-wide
/// slot is the right shape for the GUI, which has exactly one user and one
/// answer for all of them. It is the wrong shape for the *daemon*, where the
/// only handler that can reach a user is one bound to a particular routed
/// connection — the client on the other end of it. Two workspaces connecting to
/// two machines at once each need their own, and a global would give the second
/// one's prompt to the first one's socket.
///
/// A thread-local works because [`Installer`] is blocking start to finish: the
/// consent question is asked on the same thread that will write the bytes. The
/// router's relay therefore wraps its `spawn_blocking` body in this, and
/// [`install_confirm`] finds it before it looks at the global.
///
/// Nesting restores rather than clears, so a GUI-process call inside a scoped
/// one (there are none today) would not silently lose its handler.
pub fn with_install_confirm<T>(confirm: Arc<dyn InstallConfirm>, f: impl FnOnce() -> T) -> T {
    let previous = SCOPED_CONFIRM.with(|slot| slot.borrow_mut().replace(confirm));
    let out = f();
    SCOPED_CONFIRM.with(|slot| *slot.borrow_mut() = previous);
    out
}

/// The confirmation handler in force: this thread's scoped one
/// ([`with_install_confirm`]) if there is one, else the process-wide one
/// ([`set_install_confirm`]), else [`DenyInstall`].
pub fn install_confirm() -> Arc<dyn InstallConfirm> {
    if let Some(scoped) = SCOPED_CONFIRM.with(|slot| slot.borrow().clone()) {
        return scoped;
    }
    confirm_slot()
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or_else(|_| Arc::new(DenyInstall))
}

// ---------------------------------------------------------------------------
// Install progress
// ---------------------------------------------------------------------------

/// How far a first install has got, in bytes.
///
/// Only the two steps that take real time appear. `uname`, `stat`, `mkdir`,
/// `chmod` and the rename are single round trips: a phase for each would flicker
/// past faster than it could be read, and a progress display that spends most of
/// its life on two steps is better off saying which of the two it is on.
///
/// The byte counts are of the *asset*, so `Uploading` restarts at zero rather
/// than continuing where `Downloading` left off. Two bars' worth of work shown
/// as one 0-200% sweep would be worse; two named phases each running 0-100% is
/// what the user is actually waiting through.
///
/// Serialisable because the install runs in the **daemon** and the user is in
/// the GUI: this crosses the routed connection as a
/// [`RoutePrompt::InstallProgress`](crate::daemon::router::RoutePrompt) frame.
/// Unlike [`InstallRequest`] it needs no wire twin — every field is already a
/// plain number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallPhase {
    /// Fetching the asset onto *this* machine over HTTPS.
    ///
    /// `total` is `None` when the server sent no `Content-Length` — rare for a
    /// release asset, but a chunked response is legal and a progress sink that
    /// cannot represent "unknown total" would have to invent one.
    Downloading { done: u64, total: Option<u64> },
    /// Writing the verified bytes to the remote over SFTP. `total` is exact:
    /// the bytes are in memory by now.
    Uploading { done: u64, total: u64 },
}

impl InstallPhase {
    /// Fraction complete in `0.0..=1.0`, or `None` when the total is unknown.
    pub fn fraction(&self) -> Option<f32> {
        let (done, total) = match *self {
            InstallPhase::Downloading { done, total } => (done, total?),
            InstallPhase::Uploading { done, total } => (done, total),
        };
        if total == 0 {
            return None;
        }
        Some((done as f32 / total as f32).clamp(0.0, 1.0))
    }
}

/// Watches an install go by (the first install writes ~8 MB across
/// two network hops, and a client that says only "connecting…" for the length of
/// both is indistinguishable from one that has hung).
///
/// **Reports are frequent and must be cheap.** One arrives per transfer chunk —
/// hundreds over a single install — so an implementation stores the latest and
/// returns. It must not block, lock anything a UI thread holds, or do IO: the
/// thread calling this is the one moving the bytes.
///
/// **Nothing here affects the install.** It is a side channel, which is why
/// [`Installer`] reaches for it through the global rather than carrying it as a
/// field the way it carries [`InstallConfirm`] — a sink cannot change what gets
/// written, so it does not belong in the constructor every caller and fake has
/// to satisfy.
///
/// The default ([`SilentProgress`]) drops everything, which is the right
/// behaviour for a headless daemon with nobody watching.
pub trait InstallProgress: Send + Sync {
    /// `host` is the same label [`InstallRequest::host`] carries, so a sink
    /// serving several machines can tell them apart.
    fn report(&self, host: &str, phase: InstallPhase);
}

/// The default: nobody is watching, so nothing is recorded.
pub struct SilentProgress;

impl InstallProgress for SilentProgress {
    fn report(&self, _host: &str, _phase: InstallPhase) {}
}

static PROGRESS: OnceLock<Mutex<Arc<dyn InstallProgress>>> = OnceLock::new();

fn progress_slot() -> &'static Mutex<Arc<dyn InstallProgress>> {
    PROGRESS.get_or_init(|| Mutex::new(Arc::new(SilentProgress)))
}

/// Register the process-wide progress sink. Called once by the GUI at startup;
/// last call wins.
pub fn set_install_progress(progress: Arc<dyn InstallProgress>) {
    if let Ok(mut slot) = progress_slot().lock() {
        *slot = progress;
    }
}

thread_local! {
    /// A sink that outranks [`PROGRESS`] for the duration of one call, on one
    /// thread. See [`with_install_progress`].
    static SCOPED_PROGRESS: std::cell::RefCell<Option<Arc<dyn InstallProgress>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with `progress` receiving any install it drives, then put the
/// previous sink back.
///
/// The same shape, and the same reason, as [`with_install_confirm`]: in the
/// daemon the only sink that can reach a user is one bound to a particular
/// routed connection, and two machines installing at once through a global would
/// report both machines' bytes to whichever client asked last.
pub fn with_install_progress<T>(progress: Arc<dyn InstallProgress>, f: impl FnOnce() -> T) -> T {
    let previous = SCOPED_PROGRESS.with(|slot| slot.borrow_mut().replace(progress));
    let out = f();
    SCOPED_PROGRESS.with(|slot| *slot.borrow_mut() = previous);
    out
}

/// The progress sink in force: this thread's scoped one, else the process-wide
/// one, else [`SilentProgress`].
pub fn install_progress() -> Arc<dyn InstallProgress> {
    if let Some(scoped) = SCOPED_PROGRESS.with(|slot| slot.borrow().clone()) {
        return scoped;
    }
    progress_slot()
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or_else(|_| Arc::new(SilentProgress))
}

// ---------------------------------------------------------------------------
// Asking a server binary what it speaks
// ---------------------------------------------------------------------------

/// The flag that makes a `tty7-server` print [`RemoteProtocol`] and exit.
///
/// A *file*, not a running daemon: the numbers are compile-time constants, so
/// this answers "what would this binary speak" without a socket, a handshake, or
/// anything already being up. That is what lets the installer decide whether to
/// write 8 MB **before** writing it.
///
/// Servers older than this flag print usage to stderr and exit non-zero, which
/// [`Installer::probe_protocol`] reads as "no opinion" — the same conservative
/// answer an unreadable `/proc` gets.
pub const PROTOCOL_FLAG: &str = "--protocol";

/// What a `tty7-server` binary speaks, as it reports itself.
///
/// The remote counterpart of [`DaemonVersion`](crate::daemon::protocol::DaemonVersion),
/// and deliberately the same shape: two dialect numbers that decide
/// compatibility, plus a build string that decides nothing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteProtocol {
    /// [`crate::daemon::control::CONTROL_VERSION`] — the control dialect, which
    /// is what a remote *workspace* runs on.
    pub control: u32,
    /// [`crate::daemon::protocol::PROTOCOL_VERSION`] — the pane dialect, which
    /// is what a routed *pane* on that machine runs on.
    pub protocol: u32,
    /// `CARGO_PKG_VERSION`. **Display only** — same rule as
    /// [`DaemonVersion::build`](crate::daemon::protocol::DaemonVersion::build)
    /// and [`ControlHelloOk::build`](crate::daemon::control::ControlHelloOk::build).
    /// Two builds that speak the same numbers are interchangeable no matter what
    /// their version strings say, and treating a version string as a dialect is
    /// exactly the bug this type exists to end.
    pub build: String,
}

impl RemoteProtocol {
    /// What this client speaks.
    pub fn of_this_build() -> RemoteProtocol {
        RemoteProtocol {
            control: crate::daemon::control::CONTROL_VERSION,
            protocol: crate::daemon::protocol::PROTOCOL_VERSION,
            build: client_version().to_string(),
        }
    }

    /// Whether a server speaking `self` can serve a client speaking `other`.
    ///
    /// **Both numbers, both exactly equal** — the same judgement
    /// `spawn::ensure_running` makes locally (`v.protocol == PROTOCOL_VERSION`),
    /// applied to both dialects because a remote workspace uses both: control
    /// for the workspace itself, pane for every terminal in it.
    ///
    /// Equality rather than `>=` deliberately. A newer server is not
    /// automatically able to speak an older client's dialect, and guessing that
    /// it can turns a clean prompt into a wire error halfway through a session.
    pub fn serves(&self, other: &RemoteProtocol) -> bool {
        self.control == other.control && self.protocol == other.protocol
    }

    /// The single line a server prints for [`PROTOCOL_FLAG`].
    ///
    /// Paired with [`parse`](Self::parse) here rather than left to each side's
    /// own `serde_json` call: the writer is `tty7-server` and the reader is the
    /// client, they ship separately and meet over SSH, and one shared function
    /// is what stops the format drifting between them.
    pub fn to_line(&self) -> String {
        // Infallible in practice — three plain fields — and a server that could
        // not describe itself should still exit cleanly rather than make the
        // caller handle an error that cannot happen.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Parse one from a probe's stdout.
    ///
    /// Takes the **last** non-blank line: a login shell that prints a banner
    /// from `.bashrc` would otherwise poison an otherwise fine answer, and the
    /// server writes its line last because it writes it at exit.
    pub fn parse(stdout: &str) -> Option<RemoteProtocol> {
        let line = stdout.lines().rev().find(|l| !l.trim().is_empty())?;
        serde_json::from_str(line.trim()).ok()
    }
}

// ---------------------------------------------------------------------------
// Version negotiation (mirroring `spawn::ensure_running`).
// ---------------------------------------------------------------------------

/// A remote daemon that is serving a machine at a *different* build than the
/// client we are running.
///
/// The local analogue is [`crate::daemon::spawn::MismatchedDaemon`], and the
/// reasoning is identical: that daemon owns every live pane on that machine, so
/// killing it at connect time would silently destroy running work. We keep using
/// it and record the mismatch here; the GUI raises the keep-or-restart prompt and
/// calls [`restart_remote_daemon`] if the user picks restart.
///
/// Binaries coexist (the install path carries the version), so "restart" means
/// only that the *running process* is replaced — the old binary stays on disk.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MismatchedRemoteDaemon {
    /// Machine label, matching [`InstallRequest::host`].
    pub host: String,
    /// The version the running daemon's executable path encodes, or `None` when
    /// it could not be read (a locked-down `/proc`, a hand-placed binary).
    pub running_version: Option<String>,
    /// The executable path the running daemon was launched from, when known.
    pub running_exe: Option<String>,
    /// The version we installed and would rather it were.
    pub wanted_version: String,
}

static MISMATCHED: Mutex<Vec<MismatchedRemoteDaemon>> = Mutex::new(Vec::new());

thread_local! {
    /// Where mismatches found on this thread go instead of [`MISMATCHED`].
    /// See [`with_mismatch_sink`].
    static SCOPED_MISMATCH: std::cell::RefCell<Option<Arc<Mutex<Vec<MismatchedRemoteDaemon>>>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with any mismatch it discovers collected into `sink` rather than into
/// the process-wide registry.
///
/// The counterpart of [`with_install_confirm`], and for the same reason:
/// [`take_mismatched_remote_daemons`] reads a static in *this* process, so a
/// mismatch the daemon finds while opening a routed connection is invisible to
/// the GUI that asked for it. Scoped, it can be handed to the client that is
/// waiting on the other end of that very connection, which is the only party
/// that can answer "keep the old sessions or restart the service?".
pub fn with_mismatch_sink<T>(
    sink: Arc<Mutex<Vec<MismatchedRemoteDaemon>>>,
    f: impl FnOnce() -> T,
) -> T {
    let previous = SCOPED_MISMATCH.with(|slot| slot.borrow_mut().replace(sink));
    let out = f();
    SCOPED_MISMATCH.with(|slot| *slot.borrow_mut() = previous);
    out
}

fn record_mismatch(entry: MismatchedRemoteDaemon) {
    if let Some(sink) = SCOPED_MISMATCH.with(|slot| slot.borrow().clone()) {
        if let Ok(mut slot) = sink.lock()
            && !slot.iter().any(|e| e.host == entry.host)
        {
            slot.push(entry);
        }
        return;
    }
    let Ok(mut slot) = MISMATCHED.lock() else {
        return;
    };
    // One entry per host: reconnecting to the same machine repeatedly must not
    // queue up a prompt per attempt.
    if slot.iter().any(|e| e.host == entry.host) {
        return;
    }
    slot.push(entry);
}

/// File mismatches discovered in *another* process into this one's registry.
///
/// The relay's landing point (`daemon::router`): the daemon finds the mismatch,
/// the GUI is the process with the keep-or-restart prompt, and
/// [`take_mismatched_remote_daemons`] only ever reads a local static. Without
/// this the consent prompt could not fire at all.
pub fn record_remote_mismatches(entries: Vec<MismatchedRemoteDaemon>) {
    for entry in entries {
        record_mismatch(entry);
    }
}

/// Remote daemons found running at a different build than this client. Take
/// semantics, so the keep-or-restart prompt fires once per discovery rather than
/// once per window.
pub fn take_mismatched_remote_daemons() -> Vec<MismatchedRemoteDaemon> {
    MISMATCHED
        .lock()
        .map(|mut slot| std::mem::take(&mut *slot))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Errors (specific, path-bearing, never retried into a different path).
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum InstallError {
    /// `uname -sm` could not be run at all.
    Probe(String),
    /// It ran, and the answer is a machine we do not publish for.
    Unsupported(UnsupportedTarget),
    /// The remote home directory could not be resolved, so no path can be built.
    NoHome(String),
    /// The download failed (network, 404 for a tag that was never published, a
    /// proxy). Carries the URL, because "which release did it even look for" is
    /// the first question.
    Download { url: String, reason: String },
    /// sha256 verification failed. Terminal: no retry, no unverified fallback.
    Checksum(ChecksumError),
    /// A WSL install found no bundled Linux server binary in this client's own
    /// installation. Terminal, and deliberately **not** downgraded to a
    /// download: a WSL distro is served the binary the client
    /// shipped with, and silently reaching for GitHub instead would turn a
    /// packaging bug into an intermittent network failure on someone else's
    /// machine. Names every directory that was looked in, because the fix is
    /// always "the installer did not ship the file".
    MissingBundled {
        asset: &'static str,
        searched: Vec<String>,
    },
    /// The user was asked and said no.
    Declined { host: String, path: String },
    /// A write to the remote failed — full disk, read-only home, no permission.
    /// Reports the exact path and the server's own reason, and is **not**
    /// retried anywhere else ("不重试，不降级到别的路径").
    Write { path: String, reason: String },
    /// The daemon would not start, or would not answer after starting.
    Launch { reason: String },
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Probe(reason) => write!(f, "could not identify the remote machine: {reason}"),
            Self::Unsupported(target) => write!(f, "{target}"),
            Self::NoHome(reason) => {
                write!(f, "could not resolve the remote home directory: {reason}")
            }
            Self::Download { url, reason } => {
                write!(f, "could not download {url}: {reason}")
            }
            Self::Checksum(e) => write!(f, "{e}"),
            Self::MissingBundled { asset, searched } => write!(
                f,
                "this build of tty7 does not ship a Linux server binary, so it cannot \
                 install one into a WSL distribution: `{asset}` was not found in {}",
                if searched.is_empty() {
                    "any known location".to_string()
                } else {
                    searched.join(", ")
                }
            ),
            Self::Declined { host, path } => write!(
                f,
                "installing tty7-server at {path} on {host} was not confirmed; nothing was written"
            ),
            Self::Write { path, reason } => {
                write!(f, "could not write {path} on the remote machine: {reason}")
            }
            Self::Launch { reason } => write!(f, "the remote tty7-server did not start: {reason}"),
        }
    }
}

impl std::error::Error for InstallError {}

impl From<InstallError> for io::Error {
    fn from(e: InstallError) -> io::Error {
        let kind = match &e {
            InstallError::Unsupported(_) | InstallError::MissingBundled { .. } => {
                io::ErrorKind::Unsupported
            }
            InstallError::Declined { .. } => io::ErrorKind::PermissionDenied,
            InstallError::Checksum(_) => io::ErrorKind::InvalidData,
            InstallError::Launch { .. } => io::ErrorKind::TimedOut,
            _ => io::ErrorKind::Other,
        };
        io::Error::new(kind, e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Outcome.
// ---------------------------------------------------------------------------

/// What [`Installer::run`] actually did, for logs and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub asset: &'static str,
    pub paths: RemotePaths,
    /// Whether bytes were transferred (false when the right version was already
    /// installed).
    pub installed: bool,
    /// Whether the user was asked (only ever true on a machine with no prior
    /// tty7 install).
    pub confirmed: bool,
    /// Whether a daemon had to be launched (false when one was already serving).
    pub launched: bool,
    /// Set when a daemon that **cannot serve this client** is on the machine.
    ///
    /// A different build is not a mismatch — a different *dialect* is. See
    /// [`Installer::check_running_build`].
    pub mismatch: Option<MismatchedRemoteDaemon>,
    /// The already-running server this connect adopted instead of installing,
    /// when its dialects matched ours despite a different build.
    ///
    /// `Some` is the case always intended and the implementation
    /// missed: a 26.7.6 client meeting a 26.7.7 server they both speak. Recorded
    /// because "we deliberately did not install" is otherwise indistinguishable
    /// in a log from "we forgot to".
    pub reused: Option<RemoteProtocol>,
}

// ---------------------------------------------------------------------------
// The flow.
// ---------------------------------------------------------------------------

/// Runs the six steps against injected remote/network/user seams.
pub struct Installer<'a> {
    ops: &'a dyn RemoteOps,
    /// Set by [`Installer::new`]; wrapped in a [`ReleaseDownload`] at use.
    fetch: Option<&'a dyn AssetFetcher>,
    /// Set by [`Installer::with_source`], and takes precedence. Exactly one of
    /// the two is ever `Some`.
    source: Option<&'a dyn ServerBinarySource>,
    confirm: &'a dyn InstallConfirm,
    host: String,
    version: String,
    /// Overridable so tests do not spend the real budget waiting for a daemon
    /// that a fake will never start.
    startup_timeout: Duration,
    poll_interval: Duration,
}

impl<'a> Installer<'a> {
    pub fn new(
        ops: &'a dyn RemoteOps,
        fetch: &'a dyn AssetFetcher,
        confirm: &'a dyn InstallConfirm,
        host: impl Into<String>,
    ) -> Self {
        Self {
            ops,
            fetch: Some(fetch),
            source: None,
            confirm,
            host: host.into(),
            version: client_version().to_string(),
            startup_timeout: REMOTE_STARTUP_TIMEOUT,
            poll_interval: REMOTE_POLL_INTERVAL,
        }
    }

    /// The same six steps, with `source` deciding where the bytes come from:
    /// [`wsl::BundledServerBinary`] for a distro on this machine,
    /// [`BundledOrRelease`] for a real remote. A build with no HTTP client at
    /// all can still install through the former.
    pub fn with_source(
        ops: &'a dyn RemoteOps,
        source: &'a dyn ServerBinarySource,
        confirm: &'a dyn InstallConfirm,
        host: impl Into<String>,
    ) -> Self {
        Self {
            ops,
            fetch: None,
            source: Some(source),
            confirm,
            host: host.into(),
            version: client_version().to_string(),
            startup_timeout: REMOTE_STARTUP_TIMEOUT,
            poll_interval: REMOTE_POLL_INTERVAL,
        }
    }

    /// Install a specific version instead of this build's. Tests only — a real
    /// client can only speak its own dialect.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Shorten the daemon-startup budget. Tests only.
    pub fn with_timeouts(mut self, startup: Duration, poll: Duration) -> Self {
        self.startup_timeout = startup;
        self.poll_interval = poll;
        self
    }

    /// The whole flow. On `Ok`, the machine has `tty7-server-<version>` installed
    /// and a daemon answering on its control socket.
    pub fn run(&self) -> Result<InstallReport, InstallError> {
        // --- 1. uname -sm --------------------------------------------------
        let uname = self
            .ops
            .run("uname -sm")
            .map_err(InstallError::Probe)
            .and_then(|out| {
                if out.success() {
                    Ok(out.stdout)
                } else {
                    Err(InstallError::Probe(out.failure_reason()))
                }
            })?;
        let asset = asset::asset_for_uname(&uname).map_err(InstallError::Unsupported)?;

        // --- 2. is the matching version already there? ----------------------
        let home = self.ops.home_dir().map_err(InstallError::NoHome)?;
        let paths = asset::remote_paths(&home, &self.version);

        let already = self
            .ops
            .stat(&paths.binary)
            .map_err(|reason| InstallError::Write {
                path: paths.binary.clone(),
                reason,
            })?;

        let mut report = InstallReport {
            asset,
            paths: paths.clone(),
            installed: false,
            confirmed: false,
            launched: false,
            mismatch: None,
            reused: None,
        };

        // A file that exists but is not executable is a half-finished install
        // from a crashed run (rename landed, chmod did not) — redo it rather
        // than launching something the kernel will refuse.
        let usable = already.is_some_and(|stat| !stat.is_dir && stat.mode & 0o100 != 0);
        if !usable {
            // --- 3. before writing 8 MB, ask what is already serving ----------
            //
            // Version skew is settled by comparing dialects, not
            // build strings. Only the *running* server is asked: the socket is
            // singular, so a compatible binary that is merely present on disk
            // would still have to be started — and starting our own is simpler
            // and more predictable than adopting a stranger's file.
            match self.adoptable_running_server()? {
                Some((exe, spoken)) => {
                    log::info!(
                        "remote {}: adopting the running {} (control {}, protocol {}) \
                         instead of installing {} — same dialects",
                        self.host,
                        spoken.build,
                        spoken.control,
                        spoken.protocol,
                        self.version,
                    );
                    report.paths = asset::remote_paths_for_binary(&home, &exe);
                    report.reused = Some(spoken);
                }
                None => {
                    let (confirmed, _) = self.install(asset, &paths)?;
                    report.installed = true;
                    report.confirmed = confirmed;
                }
            }
        }

        // --- 6. make sure a daemon is serving --------------------------------
        let (launched, mismatch) = self.ensure_daemon(&report.paths)?;
        report.launched = launched;
        report.mismatch = mismatch;
        Ok(report)
    }

    /// The running `tty7-server` on this machine, when it speaks our dialects.
    ///
    /// `None` covers every reason not to adopt one, and they are deliberately
    /// indistinguishable to the caller: nothing running, an unreadable `/proc`,
    /// a binary too old to know [`PROTOCOL_FLAG`], or one that answered with
    /// dialects we cannot speak. All four mean "install ours", and none of them
    /// is an error — a machine we cannot interrogate is a machine we install on,
    /// exactly as before this existed.
    fn adoptable_running_server(&self) -> Result<Option<(String, RemoteProtocol)>, InstallError> {
        let Some(exe) = self.running_server_exe() else {
            return Ok(None);
        };
        let Some(spoken) = self.probe_protocol(&exe) else {
            return Ok(None);
        };
        if !spoken.serves(&RemoteProtocol::of_this_build()) {
            return Ok(None);
        }
        Ok(Some((exe, spoken)))
    }

    /// Ask a server *binary* what it speaks. `None` if it cannot say.
    ///
    /// Cheap by design: one SSH command against a file, no socket and no daemon,
    /// so it can be asked before deciding whether to transfer anything.
    fn probe_protocol(&self, exe: &str) -> Option<RemoteProtocol> {
        let cmd = format!("{} {PROTOCOL_FLAG}", shell_quote(exe));
        let out = self.ops.run(&cmd).ok()?;
        if !out.success() {
            // A server older than the flag prints usage and exits non-zero.
            return None;
        }
        RemoteProtocol::parse(&out.stdout)
    }

    /// The executable path of this user's running `tty7-server`, if any.
    fn running_server_exe(&self) -> Option<String> {
        let out = self.ops.run(RUNNING_EXE_COMMAND).ok()?;
        let exe = out.stdout.trim();
        (!exe.is_empty()).then(|| exe.to_string())
    }

    /// Steps 3–5: download, verify, confirm, upload, publish.
    fn install(
        &self,
        asset: &'static str,
        paths: &RemotePaths,
    ) -> Result<(bool, Vec<u8>), InstallError> {
        // --- 3. load + verify, both on the client ---------------------------
        //
        // Verification happens *before* the user is asked, not after: a prompt
        // for an install that could only fail its own integrity check is worse
        // than useless, and asking afterwards lets the prompt quote the exact
        // byte count and digest rather than a Content-Length promise.
        let LoadedBinary {
            bytes,
            origin: asset_url,
        } = self.load_binary(asset)?;

        // --- consent, once per machine --------------------------------------
        let confirmed = if self.is_first_install(paths) {
            let request = InstallRequest {
                host: self.host.clone(),
                version: self.version.clone(),
                asset,
                source_url: asset_url,
                remote_path: paths.binary.clone(),
                size_bytes: bytes.len() as u64,
                sha256: checksums::hex(&checksums::sha256(&bytes)),
            };
            if self.confirm.confirm(&request) == InstallDecision::Decline {
                return Err(InstallError::Declined {
                    host: self.host.clone(),
                    path: paths.binary.clone(),
                });
            }
            true
        } else {
            false
        };

        // --- 4. upload to the temp name --------------------------------------
        for dir in &paths.dir_chain {
            self.ops.mkdir(dir).map_err(|reason| InstallError::Write {
                path: dir.clone(),
                reason,
            })?;
        }
        // Best effort: a pre-existing directory we do not own (or a server that
        // refuses SETSTAT) must not block an install that will otherwise work.
        let _ = self.ops.chmod(&paths.bin_dir, DIR_MODE);

        let sink = install_progress();
        let total = bytes.len() as u64;
        self.ops
            .put_with_progress(&paths.temp, &bytes, &|done| {
                sink.report(&self.host, InstallPhase::Uploading { done, total });
            })
            .map_err(|reason| InstallError::Write {
                path: paths.temp.clone(),
                reason,
            })?;

        // --- 5. chmod then rename --------------------------------------------
        //
        // chmod *before* the rename, so the binary is never visible at its final
        // path in a non-executable state: a concurrent connect that finds
        // `tty7-server-<ver>` present would otherwise try to exec a 0644 file.
        self.ops
            .chmod(&paths.temp, BINARY_MODE)
            .map_err(|reason| InstallError::Write {
                path: paths.temp.clone(),
                reason,
            })?;
        if let Err(reason) = self.ops.rename(&paths.temp, &paths.binary) {
            // Some SFTP servers refuse a rename onto an existing name. The only
            // way that path exists here is a leftover from an interrupted run
            // (a *usable* binary short-circuits in `run`), so removing it and
            // retrying the rename is recovery, not a fallback to a different
            // location.
            let _ = self.ops.remove_file(&paths.binary);
            self.ops
                .rename(&paths.temp, &paths.binary)
                .map_err(|_| InstallError::Write {
                    path: paths.binary.clone(),
                    reason,
                })?;
        }

        Ok((confirmed, bytes))
    }

    /// Where step 3's bytes come from: the injected source if there is one,
    /// otherwise a [`ReleaseDownload`] over the injected fetcher.
    fn load_binary(&self, asset: &'static str) -> Result<LoadedBinary, InstallError> {
        let sink = install_progress();
        let on_progress = |done: u64, total: Option<u64>| {
            sink.report(&self.host, InstallPhase::Downloading { done, total });
        };
        if let Some(source) = self.source {
            return source.load_with_progress(&self.version, asset, &on_progress);
        }
        let Some(fetch) = self.fetch else {
            // Unreachable through either constructor; a plain error rather than
            // a panic because an installer is holding someone else's machine
            // open when it runs.
            return Err(InstallError::Download {
                url: String::new(),
                reason: "no binary source was configured".to_string(),
            });
        };
        ReleaseDownload { fetch }.load_with_progress(&self.version, asset, &on_progress)
    }

    /// Whether tty7 has ever written to this machine, decided from the remote's
    /// own state: an absent or empty `bin` directory means no.
    ///
    /// Erring towards *asking* — an unreadable directory counts as "first" —
    /// keeps the failure mode on the side of one extra prompt rather than one
    /// silent write to a machine nobody agreed to.
    fn is_first_install(&self, paths: &RemotePaths) -> bool {
        match self.ops.list_dir(&paths.bin_dir) {
            Ok(Some(entries)) => !entries.iter().any(|name| name.starts_with("tty7-server-")),
            Ok(None) => true,
            Err(_) => true,
        }
    }

    /// Step 6. Probe the remote control socket; if nothing answers, launch a
    /// detached daemon and probe again until it does.
    ///
    /// The probe is `tty7-server --stdio --bridge` with stdin closed: `--bridge`
    /// connects to the machine's control socket and refuses to serve in-process,
    /// so its exit status *is* the answer, and a socket file a crash left behind
    /// reads as "nothing there" rather than as a live server. Nothing here
    /// parses a frame — the protocol handshake is end-to-end between the GUI and
    /// the far server, and a second opinion about the version
    /// living down here is exactly the coupling that design forbids.
    fn ensure_daemon(
        &self,
        paths: &RemotePaths,
    ) -> Result<(bool, Option<MismatchedRemoteDaemon>), InstallError> {
        if self.daemon_is_serving(paths)? {
            return Ok((false, self.check_running_build(paths)));
        }

        self.launch_daemon(paths)?;

        let deadline = Instant::now() + self.startup_timeout;
        loop {
            if self.daemon_is_serving(paths)? {
                return Ok((true, self.check_running_build(paths)));
            }
            if Instant::now() >= deadline {
                return Err(InstallError::Launch {
                    reason: format!(
                        "{} started but nothing was answering on the control socket after {:?}",
                        paths.binary, self.startup_timeout
                    ),
                });
            }
            std::thread::sleep(self.poll_interval);
        }
    }

    fn daemon_is_serving(&self, paths: &RemotePaths) -> Result<bool, InstallError> {
        let cmd = format!(
            "{} --stdio --bridge < /dev/null",
            shell_quote(&paths.binary)
        );
        match self.ops.run(&cmd) {
            Ok(out) => Ok(out.success()),
            Err(reason) => Err(InstallError::Launch { reason }),
        }
    }

    fn launch_daemon(&self, paths: &RemotePaths) -> Result<(), InstallError> {
        let settle = self.ops.launch_settle(&paths.binary);
        self.ops
            .spawn_detached(&launch_script(&paths.binary, settle))
            .map_err(|reason| InstallError::Launch { reason })
    }

    /// Identify the daemon that is actually serving, and record a mismatch only
    /// if it **cannot speak to us**.
    ///
    /// **A different build is not a mismatch.** The rule is to compare
    /// `PROTOCOL_VERSION` and keep an older server that is compatible — the same judgement
    /// `spawn::ensure_running` makes locally, where a daemon whose `build`
    /// differs but whose `protocol` matches is reused in silence. Comparing
    /// version *strings* here is what made a 26.7.6 client prompt about a
    /// 26.7.7 server it could talk to perfectly well, and made it upload 8 MB to
    /// a machine that needed nothing.
    ///
    /// Failing to read any of it is not an error: an unreadable `/proc`, or a
    /// server too old to know [`PROTOCOL_FLAG`], means we have no opinion — and
    /// no opinion must never be reported as a mismatch.
    fn check_running_build(&self, paths: &RemotePaths) -> Option<MismatchedRemoteDaemon> {
        let exe = self.running_server_exe()?;
        let exe = exe.as_str();
        let running_version = asset::version_from_path(exe);
        if running_version.as_deref() == Some(self.version.as_str()) || exe == paths.binary {
            return None;
        }
        // A different build, so ask the only question that decides anything.
        // An unanswerable probe leaves the old behaviour in place: a server that
        // predates the flag really might not understand us, and the prompt is
        // the honest response to not knowing.
        if self
            .probe_protocol(exe)
            .is_some_and(|spoken| spoken.serves(&RemoteProtocol::of_this_build()))
        {
            log::info!(
                "remote {} is served by {exe}, a different build this client speaks to anyway",
                self.host,
            );
            return None;
        }
        let entry = MismatchedRemoteDaemon {
            host: self.host.clone(),
            running_version,
            running_exe: Some(exe.to_string()),
            wanted_version: self.version.clone(),
        };
        log::warn!(
            "remote {} is served by {} but this client is {}; keeping it and deferring to the user",
            entry.host,
            entry.running_exe.as_deref().unwrap_or("an unknown build"),
            entry.wanted_version,
        );
        record_mismatch(entry.clone());
        Some(entry)
    }

    /// Replace the running daemon with this client's build — the "restart the
    /// service" branch of the version-mismatch prompt. Every pane it is hosting
    /// dies; that is what the prompt warns about.
    pub fn restart_daemon(&self) -> Result<(), InstallError> {
        let home = self.ops.home_dir().map_err(InstallError::NoHome)?;
        let paths = asset::remote_paths(&home, &self.version);

        // SIGTERM by the pid whose executable is a tty7-server: the daemon tears
        // down like a local `Shutdown`, hanging every pane's child up with its
        // usual grace period.
        let _ = self.ops.run(TERMINATE_RUNNING_COMMAND);

        let deadline = Instant::now() + REMOTE_SHUTDOWN_TIMEOUT;
        while self.daemon_is_serving(&paths)? {
            if Instant::now() >= deadline {
                return Err(InstallError::Launch {
                    reason: format!(
                        "the running remote daemon did not stop within {REMOTE_SHUTDOWN_TIMEOUT:?}"
                    ),
                });
            }
            std::thread::sleep(self.poll_interval);
        }

        self.launch_daemon(&paths)?;
        let deadline = Instant::now() + self.startup_timeout;
        loop {
            if self.daemon_is_serving(&paths)? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(InstallError::Launch {
                    reason: format!("{} was restarted but never started answering", paths.binary),
                });
            }
            std::thread::sleep(self.poll_interval);
        }
    }
}

/// Find the executable path of this user's running `tty7-server`, if any.
///
/// `readlink /proc/<pid>/exe` is readable only for the caller's own processes,
/// which is exactly the scope wanted: one `tty7-server` per user.
/// `|| true` on the loop keeps a `set -e` login shell from turning "no daemon
/// running" into a failed command.
const RUNNING_EXE_COMMAND: &str = r#"for p in /proc/[0-9]*; do e=$(readlink "$p/exe" 2>/dev/null) || continue; case "$e" in */tty7-server-*) printf '%s' "${e% (deleted)}"; break;; esac; done; true"#;

/// SIGTERM this user's running `tty7-server`, if any.
const TERMINATE_RUNNING_COMMAND: &str = r#"for p in /proc/[0-9]*; do e=$(readlink "$p/exe" 2>/dev/null) || continue; case "$e" in */tty7-server-*) kill -TERM "${p#/proc/}" 2>/dev/null; break;; esac; done; true"#;

/// The command that starts a detached remote daemon.
///
/// `setsid` puts it in its own session so closing the SSH channel — which sends
/// SIGHUP to the session's foreground group — cannot take the daemon with it,
/// mirroring what `spawn::detach` does locally. Not every minimal image ships
/// `setsid`, so `nohup` is the fallback; both redirect all three streams to
/// `/dev/null`, without which the SSH channel would stay open holding the
/// daemon's inherited stdout for as long as the daemon lives.
fn launch_command(binary: &str) -> String {
    let bin = shell_quote(binary);
    format!(
        "if command -v setsid >/dev/null 2>&1; then \
           setsid {bin} --daemon < /dev/null > /dev/null 2>&1 & \
         else \
           nohup {bin} --daemon < /dev/null > /dev/null 2>&1 & \
         fi"
    )
}

/// The launch line, plus whatever wait the transport asked for in
/// [`RemoteOps::launch_settle`].
///
/// A free function so the one thing that must never happen — the wait replacing
/// the launch instead of following it — is testable without a machine of any
/// kind. A daemon that was never launched fails exactly like one that was
/// reaped, so this is cheap to get wrong and expensive to notice.
fn launch_script(binary: &str, settle: Option<String>) -> String {
    let launch = launch_command(binary);
    match settle {
        Some(settle) => format!("{launch}\n{settle}"),
        None => launch,
    }
}

/// POSIX single-quote escaping. Home directories with spaces, apostrophes or
/// `$` in them are rare but real, and every command here interpolates a path.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A stable label for a connection, for prompts and mismatch records.
///
/// **This string crosses the process boundary.** It is what
/// [`MismatchedRemoteDaemon::host`] carries to the GUI, and the GUI resolves it
/// back to a machine through
/// [`RouteTarget::origin_key`](crate::daemon::router::RouteTarget::origin_key),
/// which produces the same [`ConnectionKey`](crate::daemon::ssh::ConnectionKey)
/// string for an SSH target. That is how "Restart Server" finds the box it is
/// about. Changing the shape here without changing that one breaks the restart
/// silently, so the two are pinned together by
/// `the_origin_key_of_an_ssh_target_is_its_connection_key`.
fn connection_label(conn: &SshConnection) -> String {
    conn.key().as_str().to_string()
}

// ---------------------------------------------------------------------------
// The entry point B1's transport calls.
// ---------------------------------------------------------------------------

/// Make sure `conn`'s machine has this client's `tty7-server` installed and a
/// daemon serving on its control socket, and answer **where that binary is**.
///
/// **This is the seam with the SSH transport**: call it before opening a link,
/// and on `Ok` `direct-streamlocal` (or the `--stdio` fallback) has something to
/// reach. It is idempotent and cheap on the common path — an already-installed
/// binary plus a live daemon costs two SSH commands and one SFTP stat, no
/// download, no prompt.
///
/// The returned path is **absolute and version-qualified**
/// (`~/.local/share/tty7/bin/tty7-server-<version>`), and the session-channel
/// fallback must use it rather than the bare name. Nothing puts that directory
/// on a non-interactive `PATH`, and the file is not even called `tty7-server` —
/// so `exec tty7-server --stdio` is a `command not found` on a machine where the
/// install just succeeded.
///
/// A version mismatch is *not* an error: an older daemon still owns every live
/// pane on that machine, so it keeps serving and the mismatch is recorded for
/// [`take_mismatched_remote_daemons`] to raise. Only a machine we cannot install
/// on, cannot verify a download for, or cannot get a daemon running on fails.
pub fn ensure_remote_server(conn: &Arc<SshConnection>) -> io::Result<String> {
    let host = connection_label(conn);
    ensure_remote_server_labeled(conn, &host)
}

/// [`ensure_remote_server`] with an explicit machine label for the prompt (the
/// GUI knows the user's own name for a host; the connection key does not).
pub fn ensure_remote_server_labeled(conn: &Arc<SshConnection>, host: &str) -> io::Result<String> {
    let ops = ssh_ops::SshRemoteOps::new(conn.clone());
    let fetch = default_fetcher();
    let confirm = install_confirm();
    let source = BundledOrRelease::from_env(fetch.as_ref());
    let report = Installer::with_source(&ops, &source, confirm.as_ref(), host).run()?;
    log::info!(
        "remote {host}: {} at {} ({}{})",
        if report.installed {
            "installed tty7-server"
        } else {
            "tty7-server already present"
        },
        report.paths.binary,
        if report.launched {
            "daemon launched"
        } else {
            "daemon already running"
        },
        if report.mismatch.is_some() {
            ", build mismatch recorded"
        } else {
            ""
        },
    );
    Ok(report.paths.binary)
}

/// Restart the remote daemon at this client's build, dropping every pane it
/// hosts. The "restart the service" answer to the version-mismatch prompt.
pub fn restart_remote_daemon(conn: &Arc<SshConnection>) -> io::Result<()> {
    let host = connection_label(conn);
    let ops = ssh_ops::SshRemoteOps::new(conn.clone());
    let fetch = default_fetcher();
    let confirm = install_confirm();
    Installer::new(&ops, fetch.as_ref(), confirm.as_ref(), host).restart_daemon()?;
    Ok(())
}

/// The HTTPS fetcher, when this build has one.
#[cfg(feature = "remote-install")]
fn default_fetcher() -> Arc<dyn AssetFetcher> {
    Arc::new(download::HttpsFetcher::default())
}

/// A build without the `remote-install` feature — `tty7-server` itself, which
/// links no HTTP client — can still *use* an installed server, but cannot fetch
/// one. Failing here with a plain message beats failing at link time or, worse,
/// pretending the download was attempted.
#[cfg(not(feature = "remote-install"))]
fn default_fetcher() -> Arc<dyn AssetFetcher> {
    struct NoFetcher;
    impl AssetFetcher for NoFetcher {
        fn get(&self, _url: &str) -> Result<Vec<u8>, String> {
            Err(
                "this build has no HTTP client (the `remote-install` feature is off), \
                 so it cannot download a server binary"
                    .to_string(),
            )
        }
    }
    Arc::new(NoFetcher)
}

#[cfg(test)]
mod tests;

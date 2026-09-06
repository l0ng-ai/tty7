use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub mod asset;
pub mod checksums;
mod coordinator;
#[cfg(feature = "remote-install")]
pub mod download;
pub mod outcome;
#[cfg(feature = "remote-install")]
pub mod proxy;
pub mod ssh_ops;
pub mod wsl;

pub use asset::{RemotePaths, UnsupportedTarget};
pub use checksums::ChecksumError;

use crate::daemon::ssh::SshConnection;

pub fn client_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

const BINARY_MODE: u32 = 0o755;
const DIR_MODE: u32 = 0o700;

const REMOTE_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const REMOTE_POLL_INTERVAL: Duration = Duration::from_millis(400);
const REMOTE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub status: Option<u32>,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteStat {
    pub size: u64,
    pub mode: u32,
    pub is_dir: bool,
}

pub trait RemoteOps: Send + Sync {
    fn home_dir(&self) -> Result<String, String>;
    fn run(&self, cmd: &str) -> Result<ExecOutput, String>;
    fn run_with_timeout(&self, cmd: &str, _budget: Duration) -> Result<ExecOutput, String> {
        self.run(cmd)
    }
    fn spawn_detached(&self, cmd: &str) -> Result<(), String>;
    fn launch_settle(&self, _binary: &str) -> Option<String> {
        None
    }
    fn stat(&self, path: &str) -> Result<Option<RemoteStat>, String>;
    fn mkdir(&self, path: &str) -> Result<(), String>;
    fn chmod(&self, path: &str, mode: u32) -> Result<(), String>;
    fn put(&self, path: &str, bytes: &[u8]) -> Result<(), String>;
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
    fn rename(&self, from: &str, to: &str) -> Result<(), String>;
    fn remove_file(&self, path: &str) -> Result<(), String>;
    fn list_dir(&self, path: &str) -> Result<Option<Vec<String>>, String>;
}

pub trait AssetFetcher: Send + Sync {
    fn get(&self, url: &str) -> Result<Vec<u8>, String>;

    fn get_with_progress(
        &self,
        url: &str,
        on_progress: &dyn Fn(u64, Option<u64>),
    ) -> Result<Vec<u8>, String> {
        let _ = on_progress;
        self.get(url)
    }
}

pub struct LoadedBinary {
    pub bytes: Vec<u8>,
    pub origin: String,
}

impl std::fmt::Debug for LoadedBinary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedBinary")
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .field("origin", &self.origin)
            .finish()
    }
}

pub trait ServerBinarySource: Send + Sync {
    fn load(&self, version: &str, asset: &'static str) -> Result<LoadedBinary, InstallError>;

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

pub struct BundledOrRelease<'a> {
    pub fetch: &'a dyn AssetFetcher,
    pub bundled: Option<wsl::BundledServerBinary>,
    /// When a bundled directory is configured but the requested asset is absent,
    /// fall back to the release download instead of failing with `MissingBundled`.
    pub fallback_on_missing: bool,
}

impl<'a> BundledOrRelease<'a> {
    pub fn from_env(fetch: &'a dyn AssetFetcher) -> Self {
        Self {
            fetch,
            bundled: wsl::BundledServerBinary::from_env_only(),
            fallback_on_missing: false,
        }
    }

    /// Prefer a server binary shipped next to the client executable (see
    /// `wsl::BundledServerBinary::discover`), falling back to the GitHub release
    /// download when no matching bundled asset is present.
    pub fn discover(fetch: &'a dyn AssetFetcher) -> Self {
        Self {
            fetch,
            bundled: Some(wsl::BundledServerBinary::discover()),
            fallback_on_missing: true,
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
            Some(bundled) => match bundled.load(version, asset) {
                Ok(binary) => Ok(binary),
                Err(InstallError::MissingBundled { .. }) if self.fallback_on_missing => {
                    ReleaseDownload { fetch: self.fetch }.load_with_progress(
                        version,
                        asset,
                        on_progress,
                    )
                }
                Err(e) => Err(e),
            },
            None => ReleaseDownload { fetch: self.fetch }.load_with_progress(
                version,
                asset,
                on_progress,
            ),
        }
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    pub host: String,
    pub version: String,
    pub asset: &'static str,
    pub source_url: String,
    pub remote_path: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallDecision {
    Approve,
    Decline,
}

pub trait InstallConfirm: Send + Sync {
    fn confirm(&self, request: &InstallRequest) -> InstallDecision;

    fn confirm_cancellable(
        &self,
        request: &InstallRequest,
        cancellation: &super::cancel::RouteCancellation,
    ) -> InstallDecision {
        if cancellation.is_active() {
            self.confirm(request)
        } else {
            InstallDecision::Decline
        }
    }

    /// A routed caller that has gone away must not leave queued preparation
    /// or an unanswerable consent prompt running in a blocking worker.
    fn is_cancelled(&self) -> bool {
        false
    }
}

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

pub fn set_install_confirm(confirm: Arc<dyn InstallConfirm>) {
    if let Ok(mut slot) = confirm_slot().lock() {
        *slot = confirm;
    }
}

thread_local! {
            static SCOPED_CONFIRM: std::cell::RefCell<Option<Arc<dyn InstallConfirm>>> =
        const { std::cell::RefCell::new(None) };
}

pub fn with_install_confirm<T>(confirm: Arc<dyn InstallConfirm>, f: impl FnOnce() -> T) -> T {
    let previous = SCOPED_CONFIRM.with(|slot| slot.borrow_mut().replace(confirm));
    let out = f();
    SCOPED_CONFIRM.with(|slot| *slot.borrow_mut() = previous);
    out
}

pub fn install_confirm() -> Arc<dyn InstallConfirm> {
    if let Some(scoped) = SCOPED_CONFIRM.with(|slot| slot.borrow().clone()) {
        return scoped;
    }
    confirm_slot()
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or_else(|_| Arc::new(DenyInstall))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallPhase {
    Downloading { done: u64, total: Option<u64> },
    Uploading { done: u64, total: u64 },
    Restarting,
}

impl InstallPhase {
    pub fn fraction(&self) -> Option<f32> {
        let (done, total) = match *self {
            InstallPhase::Downloading { done, total } => (done, total?),
            InstallPhase::Uploading { done, total } => (done, total),
            InstallPhase::Restarting => return None,
        };
        if total == 0 {
            return None;
        }
        Some((done as f32 / total as f32).clamp(0.0, 1.0))
    }
}

pub trait InstallProgress: Send + Sync {
    fn report(&self, host: &str, phase: InstallPhase);
}

pub struct SilentProgress;

impl InstallProgress for SilentProgress {
    fn report(&self, _host: &str, _phase: InstallPhase) {}
}

static PROGRESS: OnceLock<Mutex<Arc<dyn InstallProgress>>> = OnceLock::new();

fn progress_slot() -> &'static Mutex<Arc<dyn InstallProgress>> {
    PROGRESS.get_or_init(|| Mutex::new(Arc::new(SilentProgress)))
}

pub fn set_install_progress(progress: Arc<dyn InstallProgress>) {
    if let Ok(mut slot) = progress_slot().lock() {
        *slot = progress;
    }
}

thread_local! {
            static SCOPED_PROGRESS: std::cell::RefCell<Option<Arc<dyn InstallProgress>>> =
        const { std::cell::RefCell::new(None) };
}

pub fn with_install_progress<T>(progress: Arc<dyn InstallProgress>, f: impl FnOnce() -> T) -> T {
    let previous = SCOPED_PROGRESS.with(|slot| slot.borrow_mut().replace(progress));
    let out = f();
    SCOPED_PROGRESS.with(|slot| *slot.borrow_mut() = previous);
    out
}

pub fn install_progress() -> Arc<dyn InstallProgress> {
    if let Some(scoped) = SCOPED_PROGRESS.with(|slot| slot.borrow().clone()) {
        return scoped;
    }
    progress_slot()
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or_else(|_| Arc::new(SilentProgress))
}

pub const PROTOCOL_FLAG: &str = "--protocol";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteProtocol {
    pub control: u32,
    pub protocol: u32,
    pub build: String,
}

impl RemoteProtocol {
    pub fn of_this_build() -> RemoteProtocol {
        RemoteProtocol {
            control: crate::daemon::control::CONTROL_VERSION,
            protocol: crate::daemon::protocol::PROTOCOL_VERSION,
            build: client_version().to_string(),
        }
    }

    pub fn dialect(&self) -> (u32, u32) {
        (self.control, self.protocol)
    }

    pub fn serves(&self, other: &RemoteProtocol) -> bool {
        self.control == other.control && self.protocol == other.protocol
    }

    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn parse(stdout: &str) -> Option<RemoteProtocol> {
        let line = stdout.lines().rev().find(|l| !l.trim().is_empty())?;
        serde_json::from_str(line.trim()).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MismatchedRemoteDaemon {
    pub host: String,
    pub running_version: Option<String>,
    pub running_exe: Option<String>,
    pub wanted_version: String,
}

static MISMATCHED: Mutex<Vec<MismatchedRemoteDaemon>> = Mutex::new(Vec::new());

thread_local! {
            static SCOPED_MISMATCH: std::cell::RefCell<Option<Arc<Mutex<Vec<MismatchedRemoteDaemon>>>>> =
        const { std::cell::RefCell::new(None) };
}

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
    if slot.iter().any(|e| e.host == entry.host) {
        return;
    }
    slot.push(entry);
}

pub fn record_remote_mismatches(entries: Vec<MismatchedRemoteDaemon>) {
    for entry in entries {
        record_mismatch(entry);
    }
}

/// Retires the notes a machine earned before its server was restarted or
/// replaced into this build. Keyed the way the notes are — by the route origin
/// that discovered them — so notes about other machines stay owed.
pub fn forget_remote_mismatch(host: &str) {
    if let Ok(mut slot) = MISMATCHED.lock() {
        slot.retain(|e| e.host != host);
    }
}

pub fn take_mismatched_remote_daemons() -> Vec<MismatchedRemoteDaemon> {
    MISMATCHED
        .lock()
        .map(|mut slot| std::mem::take(&mut *slot))
        .unwrap_or_default()
}

#[derive(Debug)]
pub enum InstallError {
    Cancelled,
    Probe(String),
    Unsupported(UnsupportedTarget),
    NoHome(String),
    Download {
        url: String,
        reason: String,
    },
    Checksum(ChecksumError),
    MissingBundled {
        asset: &'static str,
        searched: Vec<String>,
    },
    Declined {
        host: String,
        path: String,
    },
    Write {
        path: String,
        reason: String,
    },
    Launch {
        reason: String,
    },
    MaintenanceDeferred {
        reason: String,
    },
    /// The running server predates safe-idle maintenance, so the update can
    /// only proceed by stopping it — which ends its sessions, and no caller
    /// brought the user's confirmation for that. The UI meets this error as a
    /// string after the route boundary; the marker token is what it matches
    /// to offer that confirmation, so the token is never reworded.
    LegacyStopNeedsConsent {
        host: String,
    },
    /// Asked to restart a server this machine does not have. Nothing was
    /// stopped — see [`Installer::restart_daemon`].
    NoServerToRestart {
        host: String,
        path: String,
    },
    DialectMismatch {
        origin: String,
        wanted: RemoteProtocol,
        spoke: Option<RemoteProtocol>,
    },
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "remote preparation cancelled"),
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
            Self::MaintenanceDeferred { reason } => write!(
                f,
                "remote maintenance was deferred to preserve existing sessions: {reason}"
            ),
            Self::LegacyStopNeedsConsent { host } => write!(
                f,
                "the tty7-server running on {host} predates safe maintenance, so updating it \
                 means stopping it — which ends its sessions and needs an explicit confirmation \
                 [{}]",
                Self::LEGACY_STOP_NEEDS_CONSENT
            ),
            Self::NoServerToRestart { host, path } => write!(
                f,
                "{host} has no tty7-server at {path} for this build to start, so nothing was \
                 stopped; install the matching server there and it will be started as part of that"
            ),
            Self::DialectMismatch {
                origin,
                wanted,
                spoke,
            } => {
                let spoken = match spoke {
                    Some(s) => format!("control v{}, protocol v{}", s.control, s.protocol),
                    None => "nothing this client understands".to_string(),
                };
                write!(
                    f,
                    "this build needs a tty7-server speaking control v{} and protocol v{}, \
                     but {origin} speaks {spoken}; nothing was installed. \
                     Point {} at a directory holding a matching server binary.",
                    wanted.control,
                    wanted.protocol,
                    wsl::BUNDLED_DIR_ENV,
                )
            }
        }
    }
}

impl std::error::Error for InstallError {}

impl InstallError {
    /// Stable token inside the `LegacyStopNeedsConsent` message. The UI
    /// receives the error as a string after the route boundary and keys its
    /// localized consent prompt off this marker — never off the English.
    pub const LEGACY_STOP_NEEDS_CONSENT: &str = "legacy-stop-needs-consent";
}

/// Whether an error string coming back from the install path is the
/// legacy-stop consent question. Same idea as `control::parse_dialect_refusal`:
/// the wire carries strings, the parser restores the branch point.
pub fn legacy_stop_needs_consent(error: &str) -> bool {
    error.contains(InstallError::LEGACY_STOP_NEEDS_CONSENT)
}

impl From<InstallError> for io::Error {
    fn from(e: InstallError) -> io::Error {
        let kind = match &e {
            InstallError::Cancelled => io::ErrorKind::Interrupted,
            InstallError::Unsupported(_)
            | InstallError::MissingBundled { .. }
            | InstallError::NoServerToRestart { .. }
            | InstallError::LegacyStopNeedsConsent { .. }
            | InstallError::DialectMismatch { .. } => io::ErrorKind::Unsupported,
            InstallError::Declined { .. } => io::ErrorKind::PermissionDenied,
            InstallError::Checksum(_) => io::ErrorKind::InvalidData,
            InstallError::Launch { .. } => io::ErrorKind::TimedOut,
            _ => io::ErrorKind::Other,
        };
        io::Error::new(kind, e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub asset: &'static str,
    pub paths: RemotePaths,
    pub installed: bool,
    pub confirmed: bool,
    pub launched: bool,
    pub mismatch: Option<MismatchedRemoteDaemon>,
    pub reused: Option<RemoteProtocol>,
}

pub struct Installer<'a> {
    ops: &'a dyn RemoteOps,
    fetch: Option<&'a dyn AssetFetcher>,
    source: Option<&'a dyn ServerBinarySource>,
    confirm: &'a dyn InstallConfirm,
    host: String,
    version: String,
    dialect: RemoteProtocol,
    startup_timeout: Duration,
    shutdown_timeout: Duration,
    poll_interval: Duration,
    legacy_stop_consent: bool,
}

impl<'a> Installer<'a> {
    fn check_cancelled(&self) -> Result<(), InstallError> {
        if self.confirm.is_cancelled() {
            Err(InstallError::Cancelled)
        } else {
            Ok(())
        }
    }

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
            dialect: RemoteProtocol::of_this_build(),
            startup_timeout: REMOTE_STARTUP_TIMEOUT,
            shutdown_timeout: REMOTE_SHUTDOWN_TIMEOUT,
            poll_interval: REMOTE_POLL_INTERVAL,
            legacy_stop_consent: false,
        }
    }

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
            dialect: RemoteProtocol::of_this_build(),
            startup_timeout: REMOTE_STARTUP_TIMEOUT,
            shutdown_timeout: REMOTE_SHUTDOWN_TIMEOUT,
            poll_interval: REMOTE_POLL_INTERVAL,
            legacy_stop_consent: false,
        }
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self.dialect.build = self.version.clone();
        self
    }

    pub fn with_dialect(mut self, control: u32, protocol: u32) -> Self {
        self.dialect.control = control;
        self.dialect.protocol = protocol;
        self
    }

    fn paths_for(&self, home: &str) -> RemotePaths {
        asset::remote_paths(home, self.dialect.control, self.dialect.protocol)
    }

    pub fn replace(&self) -> Result<(), InstallError> {
        self.check_cancelled()?;
        let home = self.ops.home_dir().map_err(InstallError::NoHome)?;
        let paths = self.paths_for(&home);

        if !self.published_binary_serves_us(&paths)? {
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
            self.install(asset, &paths)?;
        }

        // Straight to the cycle: this is the one caller that has just proved
        // the binary is there, so `restart_daemon`'s guard would only spend
        // another round trip re-proving it.
        self.cycle_daemon(&paths)
    }

    fn published_binary_serves_us(&self, paths: &RemotePaths) -> Result<bool, InstallError> {
        let stat = self
            .ops
            .stat(&paths.binary)
            .map_err(|reason| InstallError::Write {
                path: paths.binary.clone(),
                reason,
            })?;
        if !stat.is_some_and(|s| !s.is_dir && s.mode & 0o100 != 0) {
            return Ok(false);
        }
        Ok(self
            .probe_protocol(&paths.binary)
            .is_some_and(|spoken| spoken.serves(&self.dialect)))
    }

    pub fn with_timeouts(mut self, startup: Duration, poll: Duration) -> Self {
        self.startup_timeout = startup;
        self.poll_interval = poll;
        self
    }

    /// How long the maintenance helper waits after the exact instance has
    /// acknowledged an idle stop. Expiry never permits a forced termination.
    pub fn with_shutdown_timeout(mut self, shutdown: Duration) -> Self {
        self.shutdown_timeout = shutdown;
        self
    }

    /// The user's explicit yes to stopping a server too old for safe-idle
    /// maintenance, knowing its sessions end. Without it a legacy server
    /// defers the update instead of losing work nobody agreed to lose.
    pub fn with_legacy_stop_consent(mut self, consent: bool) -> Self {
        self.legacy_stop_consent = consent;
        self
    }

    pub fn run(&self) -> Result<InstallReport, InstallError> {
        self.check_cancelled()?;
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

        let home = self.ops.home_dir().map_err(InstallError::NoHome)?;
        let paths = self.paths_for(&home);

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

        let usable = already.is_some_and(|stat| !stat.is_dir && stat.mode & 0o100 != 0);
        if !usable {
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
                    report.paths = asset::remote_paths_for_binary(
                        &home,
                        &exe,
                        self.dialect.control,
                        self.dialect.protocol,
                    );
                    report.reused = Some(spoken);
                }
                None => {
                    let (confirmed, _) = self.install(asset, &paths)?;
                    report.installed = true;
                    report.confirmed = confirmed;
                }
            }
        }

        let (launched, mismatch) = self.ensure_daemon(&report.paths)?;
        report.launched = launched;
        report.mismatch = mismatch;
        Ok(report)
    }

    fn adoptable_running_server(&self) -> Result<Option<(String, RemoteProtocol)>, InstallError> {
        let Some(exe) = self.running_server_exe() else {
            return Ok(None);
        };
        let Some(spoken) = self.probe_protocol(&exe) else {
            return Ok(None);
        };
        if !spoken.serves(&self.dialect) {
            return Ok(None);
        }
        Ok(Some((exe, spoken)))
    }

    fn probe_protocol(&self, exe: &str) -> Option<RemoteProtocol> {
        let cmd = format!("{} {PROTOCOL_FLAG}", shell_quote(exe));
        let out = self.ops.run(&cmd).ok()?;
        if !out.success() {
            return None;
        }
        RemoteProtocol::parse(&out.stdout)
    }

    fn running_server_exe(&self) -> Option<String> {
        let out = self.ops.run(RUNNING_EXE_COMMAND).ok()?;
        let exe = out.stdout.trim();
        (!exe.is_empty()).then(|| exe.to_string())
    }

    fn install(
        &self,
        asset: &'static str,
        paths: &RemotePaths,
    ) -> Result<(bool, Vec<u8>), InstallError> {
        self.check_cancelled()?;
        let LoadedBinary {
            bytes,
            origin: asset_url,
        } = self.load_binary(asset)?;
        self.check_cancelled()?;
        let asset_origin = asset_url.clone();

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
                self.check_cancelled()?;
                return Err(InstallError::Declined {
                    host: self.host.clone(),
                    path: paths.binary.clone(),
                });
            }
            true
        } else {
            false
        };

        for dir in &paths.dir_chain {
            self.check_cancelled()?;
            self.ops.mkdir(dir).map_err(|reason| InstallError::Write {
                path: dir.clone(),
                reason,
            })?;
        }
        let _ = self.ops.chmod(&paths.bin_dir, DIR_MODE);

        self.check_cancelled()?;
        let temp = unique_temp(&paths.temp);

        let sink = install_progress();
        let total = bytes.len() as u64;
        let publish = (|| {
            self.ops
                .put_with_progress(&temp, &bytes, &|done| {
                    sink.report(&self.host, InstallPhase::Uploading { done, total });
                })
                .map_err(|reason| InstallError::Write {
                    path: temp.clone(),
                    reason,
                })?;
            self.check_cancelled()?;
            self.ops
                .chmod(&temp, BINARY_MODE)
                .map_err(|reason| InstallError::Write {
                    path: temp.clone(),
                    reason,
                })?;
            let spoke = self.probe_protocol(&temp);
            self.check_cancelled()?;
            if !spoke.as_ref().is_some_and(|s| s.serves(&self.dialect)) {
                return Err(InstallError::DialectMismatch {
                    origin: asset_origin,
                    wanted: self.dialect.clone(),
                    spoke,
                });
            }
            self.ops
                .rename(&temp, &paths.binary)
                .map_err(|reason| InstallError::Write {
                    path: paths.binary.clone(),
                    reason,
                })
        })();
        if publish.is_err() {
            // Cancellation, partial upload and ambiguous publication failures
            // all clean only our unique staging file, never the published one.
            let _ = self.ops.remove_file(&temp);
        }
        publish?;

        Ok((confirmed, bytes))
    }

    fn load_binary(&self, asset: &'static str) -> Result<LoadedBinary, InstallError> {
        let sink = install_progress();
        let on_progress = |done: u64, total: Option<u64>| {
            sink.report(&self.host, InstallPhase::Downloading { done, total });
        };
        if let Some(source) = self.source {
            return source.load_with_progress(&self.version, asset, &on_progress);
        }
        let Some(fetch) = self.fetch else {
            return Err(InstallError::Download {
                url: String::new(),
                reason: "no binary source was configured".to_string(),
            });
        };
        ReleaseDownload { fetch }.load_with_progress(&self.version, asset, &on_progress)
    }

    fn is_first_install(&self, paths: &RemotePaths) -> bool {
        match self.ops.list_dir(&paths.bin_dir) {
            Ok(Some(entries)) => !entries.iter().any(|name| name.starts_with("tty7-server-")),
            Ok(None) => true,
            Err(_) => true,
        }
    }

    fn ensure_daemon(
        &self,
        paths: &RemotePaths,
    ) -> Result<(bool, Option<MismatchedRemoteDaemon>), InstallError> {
        self.check_cancelled()?;
        if self.daemon_is_serving(paths)? {
            let deadline = Instant::now() + self.startup_timeout;
            let mut health = self.check_health(paths, false, self.startup_timeout);
            if health.is_ok() {
                return Ok((false, None));
            }
            let mismatch = self.check_running_build(paths);
            if mismatch.is_some() {
                return Ok((false, mismatch));
            }
            // Another connect may have started this instance milliseconds ago:
            // binding the control socket alone is not its ready barrier.
            while health.is_err() && Instant::now() < deadline {
                self.check_cancelled()?;
                std::thread::sleep(
                    self.poll_interval
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
                health = self.check_health(
                    paths,
                    false,
                    deadline
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(1)),
                );
            }
            health.map_err(|reason| InstallError::Launch { reason: format!(
                "an existing endpoint answered, but the daemon is not ready: {reason}; no replacement was started"
            ) })?;
            return Ok((false, None));
        }

        self.check_cancelled()?;
        install_progress().report(&self.host, InstallPhase::Restarting);
        let launch = self.launch_daemon(paths)?;
        let deadline = Instant::now() + self.startup_timeout;
        loop {
            self.check_cancelled()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            let health = self.check_health(
                paths,
                false,
                remaining
                    .min(Duration::from_secs(3))
                    .max(Duration::from_millis(1)),
            );
            if health.is_ok() {
                return Ok((true, None));
            }
            let diagnostic =
                launch.diagnostic(self.ops, deadline.saturating_duration_since(Instant::now()));
            if diagnostic.exited_unsuccessfully || Instant::now() >= deadline {
                return Err(InstallError::Launch {
                    reason: format!(
                        "{} did not become ready: {}\n{}\nStartup log: {}",
                        paths.binary,
                        health.unwrap_err(),
                        diagnostic.text,
                        launch.log,
                    ),
                });
            }
            std::thread::sleep(
                self.poll_interval
                    .min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }

    fn daemon_is_serving(&self, paths: &RemotePaths) -> Result<bool, InstallError> {
        let cmd = format!(
            "{} --stdio --bridge < /dev/null",
            shell_quote(&paths.binary)
        );
        match self.ops.run_with_timeout(&cmd, self.startup_timeout) {
            Ok(out) => Ok(out.success()),
            Err(reason) => Err(InstallError::Launch { reason }),
        }
    }

    fn launch_daemon(&self, paths: &RemotePaths) -> Result<StartupLog, InstallError> {
        let launch = StartupLog::new(&paths.binary);
        let settle = self.ops.launch_settle(&paths.binary);
        self.ops
            .spawn_detached(&launch_script(&paths.binary, &launch, settle))
            .map_err(|reason| InstallError::Launch { reason })?;
        Ok(launch)
    }

    fn check_health(
        &self,
        paths: &RemotePaths,
        exact_build: bool,
        budget: Duration,
    ) -> Result<(), String> {
        let flag = if exact_build {
            super::maintenance::HEALTH_FLAG
        } else {
            super::maintenance::SERVING_FLAG
        };
        let command = format!(
            "{} {flag} --wait-ms {}",
            shell_quote(&paths.binary),
            budget.as_millis().max(1)
        );
        let out = self.ops.run_with_timeout(&command, budget)?;
        if !out.success() {
            return Err(out.failure_reason());
        }
        match super::maintenance::Reply::parse(&out.stdout) {
            Some(super::maintenance::Reply::Healthy {
                control,
                protocol,
                build,
                instance,
            }) if control == self.dialect.control
                && protocol == self.dialect.protocol
                && (!exact_build || build == self.version)
                && !instance.is_empty() =>
            {
                Ok(())
            }
            _ => Err(
                "the endpoints did not prove the requested dialect and one daemon instance".into(),
            ),
        }
    }

    fn check_running_build(&self, paths: &RemotePaths) -> Option<MismatchedRemoteDaemon> {
        let exe = self.running_server_exe()?;
        let exe = exe.as_str();
        if asset::dialect_from_path(exe) == Some(self.dialect.dialect()) || exe == paths.binary {
            return None;
        }
        let spoken = self.probe_protocol(exe);
        if spoken.as_ref().is_some_and(|s| s.serves(&self.dialect)) {
            log::info!(
                "remote {} is served by {exe}, a different build this client speaks to anyway",
                self.host,
            );
            return None;
        }
        let entry = MismatchedRemoteDaemon {
            host: self.host.clone(),
            running_version: spoken.map(|s| s.build),
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

    /// Start the matching candidate after an endpoint-scoped, idle-only stop.
    /// Unsupported old servers and busy servers are left running. Having a
    /// replacement binary is necessary, but never authorizes killing sessions.
    pub fn restart_daemon(&self) -> Result<(), InstallError> {
        self.check_cancelled()?;
        let home = self.ops.home_dir().map_err(InstallError::NoHome)?;
        let paths = self.paths_for(&home);
        if !self.published_binary_serves_us(&paths)? {
            return Err(InstallError::NoServerToRestart {
                host: self.host.clone(),
                path: paths.binary.clone(),
            });
        }
        self.cycle_daemon(&paths)
    }

    /// The restart itself, for callers that have just put the binary there and
    /// do not need to be told again that it is there.
    fn cycle_daemon(&self, paths: &RemotePaths) -> Result<(), InstallError> {
        // Cancellation is honoured before stopping anything. Once an explicit
        // maintenance operation has stopped the daemon, finish the start even
        // if its window closes; abandoning the machine halfway is not undo.
        self.check_cancelled()?;
        install_progress().report(&self.host, InstallPhase::Restarting);

        let command = format!(
            "{} {} --wait-ms {}",
            shell_quote(&paths.binary),
            super::maintenance::PREPARE_FLAG,
            self.shutdown_timeout.as_millis()
        );
        let stopped = self
            .ops
            .run(&command)
            .map_err(|reason| InstallError::MaintenanceDeferred { reason })?;
        if !stopped.success() {
            // A server too old to answer the safe-idle question can still
            // leave gracefully — its SIGTERM handler has drained panes and
            // stored scrollback since long before the feature gate. But that
            // stop ends its sessions, so it runs only when the caller carries
            // the user's explicit yes, and even then through the recorded
            // pid's identity check, never a process-name match.
            let legacy = matches!(
                super::maintenance::Reply::parse(&stopped.stdout),
                Some(super::maintenance::Reply::Deferred {
                    kind: super::maintenance::DeferredKind::Unsupported,
                    ..
                })
            );
            if !legacy {
                return Err(InstallError::MaintenanceDeferred {
                    reason: stopped.failure_reason(),
                });
            }
            if !self.legacy_stop_consent {
                return Err(InstallError::LegacyStopNeedsConsent {
                    host: self.host.clone(),
                });
            }
            self.stop_recorded_daemon(paths)?;
        } else if super::maintenance::Reply::parse(&stopped.stdout)
            != Some(super::maintenance::Reply::Stopped)
        {
            return Err(InstallError::MaintenanceDeferred { reason: "the candidate did not acknowledge a safe idle stop; no forced shutdown was attempted".into() });
        }

        self.launch_daemon(paths)?;
        let deadline = Instant::now() + self.startup_timeout;
        loop {
            // Socket existence or an EOF bridge is not a health check. The
            // candidate verifies Version, control Hello/Ping and one instance.
            let command = format!(
                "{} {}",
                shell_quote(&paths.binary),
                super::maintenance::HEALTH_FLAG
            );
            let result = self.ops.run(&command);
            let failure = match result {
                Ok(out) if out.success() => match super::maintenance::Reply::parse(&out.stdout) {
                    Some(super::maintenance::Reply::Healthy {
                        control,
                        protocol,
                        build,
                        instance,
                    }) if control == self.dialect.control
                        && protocol == self.dialect.protocol
                        && build == self.version
                        && !instance.is_empty() =>
                    {
                        None
                    }
                    _ => Some(
                        "the running endpoints did not prove the requested build and dialect"
                            .to_string(),
                    ),
                },
                Ok(out) => Some(out.failure_reason()),
                Err(reason) => Some(reason),
            };
            if failure.is_none() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(InstallError::Launch {
                    reason: format!(
                        "{} did not pass its running health check: {}",
                        paths.binary,
                        failure.unwrap()
                    ),
                });
            }
            std::thread::sleep(self.poll_interval);
        }
    }

    /// Ask the pidfile-recorded server to exit, through the candidate
    /// binary's own identity checks. Reaching here means the running server
    /// predates safe-idle maintenance AND the caller carries the user's
    /// explicit yes to ending its sessions; anything the candidate cannot
    /// verify defers the whole update rather than signalling on doubt.
    fn stop_recorded_daemon(&self, paths: &RemotePaths) -> Result<(), InstallError> {
        let command = format!(
            "{} {} --wait-ms {}",
            shell_quote(&paths.binary),
            super::maintenance::STOP_RECORDED_FLAG,
            self.shutdown_timeout.as_millis()
        );
        let stopped = self
            .ops
            .run(&command)
            .map_err(|reason| InstallError::MaintenanceDeferred { reason })?;
        if !stopped.success()
            || super::maintenance::Reply::parse(&stopped.stdout)
                != Some(super::maintenance::Reply::Stopped)
        {
            return Err(InstallError::MaintenanceDeferred {
                reason: format!(
                    "the legacy server stop was not confirmed; nothing was forced: {}",
                    stopped.failure_reason()
                ),
            });
        }
        Ok(())
    }
}

/// Finding the running server takes two shapes because `/proc` is a Linux
/// thing. On Linux, `/proc/<pid>/exe` is the honest answer: a symlink to the
/// file that is actually executing, whatever anyone did to `argv[0]`. macOS —
/// the only other machine [`asset::asset_for_uname`] will install onto — has no
/// `/proc` at all, so fall back to `ps`.
///
/// Its `comm` is not quite `exe`'s equal: on Darwin it reports `argv[0]`, so it
/// is a path only because [`launch_command`] launches by absolute path, and a
/// server started some other way would be invisible to it. It is still the
/// better of the two answers available there. Linux's `comm` is not an answer
/// at all — the name truncated to 15 characters, one short of
/// `tty7-server-c8p6` — which is why the fallback stays a fallback and `/proc`
/// keeps first refusal.
///
/// Neither arm reaches past the connecting user: `readlink` on another user's
/// `exe` is refused, and `ps` without `-A` lists only our own processes. The
/// pattern is anchored at a `/` so it cannot match a name that merely ends in
/// one of ours.
///
/// The `/proc` glob lives *inside* the `[ -d /proc ]` arm on purpose. The far
/// end runs these through the user's login shell, and zsh — the default on
/// macOS — aborts the whole command line when a glob matches nothing, so with
/// the loop at top level the trailing `true` never ran and every one of these
/// was a silent no-op on every Mac. That is what left `restart_daemon` waiting
/// out its ten seconds for a daemon nobody had asked to stop.
const RUNNING_EXE_COMMAND: &str = r#"if [ -d /proc ]; then for p in /proc/[0-9]*; do e=$(readlink "$p/exe" 2>/dev/null) || continue; case "$e" in */tty7-server-*) printf '%s' "${e% (deleted)}"; break;; esac; done; else ps -xwwo pid=,comm= 2>/dev/null | while read -r pid e; do case "$e" in */tty7-server-*) printf '%s' "$e"; break;; esac; done; fi; true"#;

struct StartupLog {
    log: String,
    exit: String,
}
struct StartupDiagnostic {
    exited_unsuccessfully: bool,
    text: String,
}

impl StartupLog {
    fn new(binary: &str) -> Self {
        let log = format!("{binary}.startup.{}.log", uuid::Uuid::new_v4());
        Self {
            exit: format!("{log}.exit"),
            log,
        }
    }
    fn diagnostic(&self, ops: &dyn RemoteOps, budget: Duration) -> StartupDiagnostic {
        let command = format!(
            "if test -s {exit}; then printf 'TTY7_STARTUP_EXIT='; head -c 16 {exit}; fi; tail -c 4096 {log}",
            exit = shell_quote(&self.exit),
            log = shell_quote(&self.log)
        );
        let text = ops
            .run_with_timeout(&command, budget.max(Duration::from_millis(1)))
            .map(|out| out.stdout)
            .unwrap_or_default();
        let exit = text
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("TTY7_STARTUP_EXIT="))
            .and_then(|code| code.parse::<i32>().ok());
        StartupDiagnostic {
            exited_unsuccessfully: exit.is_some_and(|code| code != 0),
            text,
        }
    }
}

fn launch_command(binary: &str, launch: &StartupLog) -> String {
    let log = shell_quote(&launch.log);
    let status = shell_quote(&launch.exit);
    let supervised = shell_quote(&format!(
        "{} --daemon; result=$?; printf '%s\\n' \"$result\" > {status}; exit \"$result\"",
        shell_quote(binary)
    ));
    format!(
        "umask 077; (set -C; : > {log} && : > {status}) || exit 1; \
         if command -v setsid >/dev/null 2>&1; then \
           nohup setsid sh -c {supervised} < /dev/null >> {log} 2>&1 & \
         else \
           nohup sh -c {supervised} < /dev/null >> {log} 2>&1 & \
         fi"
    )
}

fn launch_script(binary: &str, logs: &StartupLog, settle: Option<String>) -> String {
    let launch = launch_command(binary, logs);
    match settle {
        Some(settle) => format!("{launch}\n{settle}"),
        None => launch,
    }
}

fn unique_temp(shared: &str) -> String {
    let pid = std::process::id();
    let attempt = uuid::Uuid::new_v4();
    match shared.strip_suffix(".tmp") {
        Some(stem) => format!("{stem}.{pid}.{attempt}.tmp"),
        None => format!("{shared}.{pid}.{attempt}"),
    }
}

// All SSH preparation/maintenance entry points share this lock, including
// connections re-created with the same key. It is intentionally not keyed by
// the display label. Cross-client coordination needs a remote lock as well;
// unique staging names and non-destructive publication remain necessary.
fn install_lock(key: &str) -> Arc<coordinator::PreparationBatch<InstallReport>> {
    type Batch = coordinator::PreparationBatch<InstallReport>;
    static LOCKS: Mutex<Vec<(String, std::sync::Weak<Batch>)>> = Mutex::new(Vec::new());
    let mut locks = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    locks.retain(|(_, lock)| lock.strong_count() > 0);
    if let Some(lock) = locks
        .iter()
        .find(|(known, _)| known == key)
        .and_then(|(_, lock)| lock.upgrade())
    {
        return lock;
    }
    let lock = Arc::new(Batch::default());
    locks.push((key.to_string(), Arc::downgrade(&lock)));
    lock
}

pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn connection_label(conn: &SshConnection) -> String {
    conn.key().as_str().to_string()
}

pub fn ensure_remote_server(conn: &Arc<SshConnection>) -> io::Result<String> {
    let host = connection_label(conn);
    ensure_remote_server_labeled(conn, &host)
}

pub fn ensure_remote_server_labeled(conn: &Arc<SshConnection>, host: &str) -> io::Result<String> {
    let lock = install_lock(conn.key().as_str());
    let confirm = install_confirm();
    let report = lock.prepare(conn.generation, &|| confirm.is_cancelled(), || {
        let ops = ssh_ops::SshRemoteOps::new(conn.clone());
        let fetch = default_fetcher();
        let source = BundledOrRelease::discover(fetch.as_ref());
        Ok(Installer::with_source(&ops, &source, confirm.as_ref(), host).run()?)
    })?;
    // A coalesced caller still owes the mismatch to its own route/window.
    if let Some(mut mismatch) = report.mismatch.clone() {
        mismatch.host = host.to_string();
        record_mismatch(mismatch);
    }
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

pub fn restart_remote_daemon(conn: &Arc<SshConnection>) -> io::Result<()> {
    restart_remote_daemon_consenting(conn, false)
}

pub fn restart_remote_daemon_consenting(
    conn: &Arc<SshConnection>,
    legacy_stop: bool,
) -> io::Result<()> {
    let lock = install_lock(conn.key().as_str());
    let confirm = install_confirm();
    lock.maintain(&|| confirm.is_cancelled(), || {
        let host = connection_label(conn);
        let ops = ssh_ops::SshRemoteOps::new(conn.clone());
        let fetch = default_fetcher();
        Installer::new(&ops, fetch.as_ref(), confirm.as_ref(), host)
            .with_legacy_stop_consent(legacy_stop)
            .restart_daemon()?;
        Ok(())
    })
}

pub fn replace_remote_server(conn: &Arc<SshConnection>) -> io::Result<()> {
    replace_remote_server_consenting(conn, false)
}

pub fn replace_remote_server_consenting(
    conn: &Arc<SshConnection>,
    legacy_stop: bool,
) -> io::Result<()> {
    let lock = install_lock(conn.key().as_str());
    let confirm = install_confirm();
    lock.maintain(&|| confirm.is_cancelled(), || {
        let host = connection_label(conn);
        let ops = ssh_ops::SshRemoteOps::new(conn.clone());
        let fetch = default_fetcher();
        let source = BundledOrRelease::discover(fetch.as_ref());
        Installer::with_source(&ops, &source, confirm.as_ref(), host)
            .with_legacy_stop_consent(legacy_stop)
            .replace()?;
        Ok(())
    })
}

#[cfg(feature = "remote-install")]
fn default_fetcher() -> Arc<dyn AssetFetcher> {
    // Re-read rather than cache: the GUI writes `config.json` whenever the
    // proxy setting changes, and installs are rare enough for a file read.
    let manual = crate::core::config::Config::load().http_proxy;
    Arc::new(download::HttpsFetcher::new(manual.as_deref()))
}

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

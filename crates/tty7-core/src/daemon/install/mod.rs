use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub mod asset;
pub mod checksums;
#[cfg(feature = "remote-install")]
pub mod download;
pub mod outcome;
#[cfg(feature = "remote-install")]
pub mod proxy;
pub mod ssh_ops;
pub mod windows_host;
pub mod wsl;

pub use asset::{RemotePaths, RemotePlatform, UnsupportedTarget};
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
    Downloading {
        done: u64,
        total: Option<u64>,
    },
    Uploading {
        done: u64,
        total: u64,
    },
    /// The bytes are there and the far end is being waited on. Both a first
    /// install and a restart end here, which is why its caption says starting
    /// rather than restarting.
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

impl From<InstallError> for io::Error {
    fn from(e: InstallError) -> io::Error {
        let kind = match &e {
            InstallError::Unsupported(_)
            | InstallError::MissingBundled { .. }
            | InstallError::NoServerToRestart { .. }
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
            dialect: RemoteProtocol::of_this_build(),
            startup_timeout: REMOTE_STARTUP_TIMEOUT,
            shutdown_timeout: REMOTE_SHUTDOWN_TIMEOUT,
            poll_interval: REMOTE_POLL_INTERVAL,
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

    fn paths_for(&self, platform: RemotePlatform, home: &str) -> RemotePaths {
        asset::remote_paths_on(platform, home, self.dialect.control, self.dialect.protocol)
    }

    /// Which server this machine needs, and what kind of machine it is.
    ///
    /// `uname -sm` first, exactly as before, so a Linux or macOS host is asked
    /// nothing new. Only when that fails — `cmd.exe` and PowerShell have no
    /// `uname` — or answers from a POSIX layer on Windows (Git for Windows,
    /// MSYS2, Cygwin) is the Windows probe tried. And if that does not answer
    /// either, the error is `uname`'s: on a machine that is neither, it is the
    /// more useful of the two to read.
    fn detect_target(&self) -> Result<(RemotePlatform, &'static str), InstallError> {
        let unix_error = match self.ops.run("uname -sm") {
            Ok(out) if out.success() => match asset::asset_for_uname(&out.stdout) {
                Ok(asset) => return Ok((RemotePlatform::Unix, asset)),
                Err(target) if asset::uname_reports_windows(&out.stdout) => {
                    InstallError::Unsupported(target)
                }
                Err(target) => return Err(InstallError::Unsupported(target)),
            },
            Ok(out) => InstallError::Probe(out.failure_reason()),
            Err(reason) => InstallError::Probe(reason),
        };
        let arch = self
            .ops
            .run(&windows_host::probe_command())
            .ok()
            .filter(ExecOutput::success)
            .and_then(|out| windows_host::parse_probe(&out.stdout));
        match arch {
            Some(arch) => asset::asset_for_windows_arch(&arch)
                .map(|asset| (RemotePlatform::Windows, asset))
                .map_err(InstallError::Unsupported),
            None => Err(unix_error),
        }
    }

    /// The platform the probe found has to be the one SFTP is serving, or the
    /// binary would land where the shell running it cannot see it.
    ///
    /// The case this exists for is a Windows host whose OpenSSH `DefaultShell`
    /// is WSL's `bash.exe`: `uname` answers Linux, while SFTP is Windows's and
    /// writes to `C:\`. Installing a Linux server at `/C:/Users/…` would leave a
    /// file no Linux path reaches.
    fn check_platform(&self, detected: RemotePlatform, home: &str) -> Result<(), InstallError> {
        let served = RemotePlatform::of_sftp_home(home);
        if detected == served {
            return Ok(());
        }
        Err(InstallError::Probe(format!(
            "the login shell answers like a {detected:?} machine, but SFTP reports a home of \
             {home:?}, which is a {served:?} path; a remote workspace needs the shell and SFTP \
             to see the same file system (on Windows, set OpenSSH's DefaultShell back to \
             cmd.exe or PowerShell)"
        )))
    }

    pub fn replace(&self) -> Result<(), InstallError> {
        self.put_ours_and_cycle(false)
    }

    /// [`Installer::replace`] without the "already there" shortcut: upload this
    /// build's server over whatever sits at the dialect's path, then restart.
    ///
    /// The shortcut is right for a mismatch and wrong for an update. The path
    /// is named after the dialect alone, so a server from any build that
    /// speaks it passes the check — and between two dialect bumps that is
    /// every build, which is how a host keeps running the first server ever
    /// installed on it while daemon-side fixes pile up on this machine. The
    /// upload goes through the same temp-name-then-rename as a first install,
    /// so the running server keeps its file until the new one is proven to
    /// speak our dialect.
    pub fn replace_forced(&self) -> Result<(), InstallError> {
        self.put_ours_and_cycle(true)
    }

    fn put_ours_and_cycle(&self, force: bool) -> Result<(), InstallError> {
        let home = self.ops.home_dir().map_err(InstallError::NoHome)?;
        let paths = self.paths_for(RemotePlatform::of_sftp_home(&home), &home);

        if force || !self.published_binary_serves_us(&paths)? {
            let (platform, asset) = self.detect_target()?;
            self.check_platform(platform, &home)?;
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
        if !stat.is_some_and(|s| runnable(paths.platform, s)) {
            return Ok(false);
        }
        Ok(self
            .probe_protocol(paths.platform, &paths.binary)
            .is_some_and(|spoken| spoken.serves(&self.dialect)))
    }

    pub fn with_timeouts(mut self, startup: Duration, poll: Duration) -> Self {
        self.startup_timeout = startup;
        self.poll_interval = poll;
        self
    }

    /// How long [`Installer::cycle_daemon`] waits for the old server to go
    /// away. Its own knob rather than a third argument to `with_timeouts`,
    /// because the only caller that shortens it is the test that watches a
    /// stop fail, and every other caller wants the shipped ten seconds.
    pub fn with_shutdown_timeout(mut self, shutdown: Duration) -> Self {
        self.shutdown_timeout = shutdown;
        self
    }

    pub fn run(&self) -> Result<InstallReport, InstallError> {
        let (platform, asset) = self.detect_target()?;

        let home = self.ops.home_dir().map_err(InstallError::NoHome)?;
        self.check_platform(platform, &home)?;
        let paths = self.paths_for(platform, &home);

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

        let usable = already.is_some_and(|stat| runnable(platform, stat));
        if !usable {
            match self.adoptable_running_server(platform)? {
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
                    report.paths = asset::remote_paths_for_binary_on(
                        platform,
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
                    // The upload's caption was the last thing reported, so the
                    // strip sat on "copying… 100%" through the startup wait —
                    // and when the wait failed, that is the frame the user was
                    // left looking at.
                    //
                    // Reported here rather than in the wait itself: an
                    // `ensure_daemon` with nothing to install has to stay
                    // silent. A pane route quietly restarting a dead remote
                    // daemon goes through that path, and nothing on it retires
                    // a progress entry — one left there freezes the strip and
                    // takes its button away for the rest of the session.
                    install_progress().report(&self.host, InstallPhase::Restarting);
                }
            }
        }

        let (launched, mismatch) = self.ensure_daemon(&report.paths)?;
        report.launched = launched;
        report.mismatch = mismatch;
        Ok(report)
    }

    /// The answer [`Installer::run`] gives for a machine that already has this
    /// dialect's server installed, serving, and the one running — found in one
    /// round trip instead of the half dozen `run` spends finding it out.
    ///
    /// That is the answer on every connect to a machine tty7 has been to before,
    /// and `run` pays for it in sequence: `uname`, a home lookup and a `stat`
    /// over SFTP, a control probe, a process scan. Six round trips before the
    /// first pane — and the SFTP session it opens stays open for the life of
    /// the connection, one of the handful of sessions the server allows it.
    ///
    /// `None` means "go and ask properly", never "something is wrong": any
    /// other state — nothing installed, nothing serving, another build running,
    /// a remote shell that could not read the script — is `run`'s to handle.
    pub fn quick_prove(&self) -> Option<ProvedServer> {
        let out = self.ops.run(&self.quick_prove_command()).ok()?;
        if !out.success() {
            return None;
        }
        self.quick_verdict(&out.stdout)
    }

    fn quick_prove_command(&self) -> String {
        let binary = format!(
            "{}/{}",
            asset::INSTALL_DIR_COMPONENTS.join("/"),
            asset::binary_name(self.dialect.control, self.dialect.protocol)
        );
        // `sh -c`, because the login shell over there need not be a POSIX one.
        let script = format!(
            r#"b="$HOME/{binary}"; if [ -f "$b" ] && [ -x "$b" ] && "$b" --stdio --bridge </dev/null >/dev/null 2>&1; then printf '{QUICK_PROVE_MARK}%s\n' "$b"; {RUNNING_EXE_COMMAND}; fi"#
        );
        format!("sh -c {}", shell_quote(&script))
    }

    fn quick_verdict(&self, stdout: &str) -> Option<ProvedServer> {
        let (first, rest) = stdout.split_once('\n')?;
        let binary = first.strip_prefix(QUICK_PROVE_MARK)?;
        if !binary.starts_with('/') {
            return None;
        }
        // The same test `check_running_build` applies: whatever is running is
        // either this very file or a build of the same dialect. Anything else
        // is a mismatch to raise, and raising it is `run`'s job.
        let running = rest.trim();
        let ours = running.is_empty()
            || running == binary
            || asset::dialect_from_path(running) == Some(self.dialect.dialect());
        ours.then(|| ProvedServer {
            binary: binary.to_string(),
            mismatch: None,
        })
    }

    fn adoptable_running_server(
        &self,
        platform: RemotePlatform,
    ) -> Result<Option<(String, RemoteProtocol)>, InstallError> {
        let Some(exe) = self.running_server_exe(platform) else {
            return Ok(None);
        };
        let Some(spoken) = self.probe_protocol(platform, &exe) else {
            return Ok(None);
        };
        if !spoken.serves(&self.dialect) {
            return Ok(None);
        }
        Ok(Some((exe, spoken)))
    }

    fn probe_protocol(&self, platform: RemotePlatform, exe: &str) -> Option<RemoteProtocol> {
        let cmd = match platform {
            RemotePlatform::Unix => format!("{} {PROTOCOL_FLAG}", shell_quote(exe)),
            RemotePlatform::Windows => windows_host::protocol_command(exe),
        };
        let out = self.ops.run(&cmd).ok()?;
        if !out.success() {
            return None;
        }
        RemoteProtocol::parse(&out.stdout)
    }

    /// The running server's image, in the same spelling as the paths the
    /// installer keeps — on Windows the native path the process list reports
    /// is turned back into SFTP's, so it compares equal to ours.
    fn running_server_exe(&self, platform: RemotePlatform) -> Option<String> {
        match platform {
            RemotePlatform::Unix => {
                let out = self.ops.run(RUNNING_EXE_COMMAND).ok()?;
                let exe = out.stdout.trim();
                (!exe.is_empty()).then(|| exe.to_string())
            }
            RemotePlatform::Windows => {
                let out = self.ops.run(&windows_host::running_exe_command()).ok()?;
                asset::sftp_path_from_windows(out.stdout.trim())
            }
        }
    }

    fn install(
        &self,
        asset: &'static str,
        paths: &RemotePaths,
    ) -> Result<(bool, Vec<u8>), InstallError> {
        let LoadedBinary {
            bytes,
            origin: asset_url,
        } = self.load_binary(asset)?;
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
            self.ops.mkdir(dir).map_err(|reason| InstallError::Write {
                path: dir.clone(),
                reason,
            })?;
        }
        match paths.platform {
            RemotePlatform::Unix => {
                let _ = self.ops.chmod(&paths.bin_dir, DIR_MODE);
            }
            // Mode bits mean nothing to NTFS; what needs doing there instead is
            // clearing out the images earlier upgrades had to move aside.
            RemotePlatform::Windows => self.sweep_moved_aside(paths),
        }

        let temp = unique_temp(&paths.temp);

        let sink = install_progress();
        let total = bytes.len() as u64;
        self.ops
            .put_with_progress(&temp, &bytes, &|done| {
                sink.report(&self.host, InstallPhase::Uploading { done, total });
            })
            .map_err(|reason| InstallError::Write {
                path: temp.clone(),
                reason,
            })?;

        if paths.platform == RemotePlatform::Unix {
            self.ops
                .chmod(&temp, BINARY_MODE)
                .map_err(|reason| InstallError::Write {
                    path: temp.clone(),
                    reason,
                })?;
        }

        let spoke = self.probe_protocol(paths.platform, &temp);
        if !spoke.as_ref().is_some_and(|s| s.serves(&self.dialect)) {
            let _ = self.ops.remove_file(&temp);
            return Err(InstallError::DialectMismatch {
                origin: asset_origin,
                wanted: self.dialect.clone(),
                spoke,
            });
        }

        if let Err(reason) = self.ops.rename(&temp, &paths.binary) {
            match paths.platform {
                RemotePlatform::Unix => {
                    let _ = self.ops.remove_file(&paths.binary);
                }
                // Windows will not delete an image that is running — and the
                // one at this path is running whenever this is an update — but
                // it will rename one. Moving it aside frees the name while the
                // old daemon keeps executing from the moved file until the
                // restart that follows.
                RemotePlatform::Windows => {
                    let aside = moved_aside(&paths.binary);
                    if self.ops.rename(&paths.binary, &aside).is_err() {
                        let _ = self.ops.remove_file(&paths.binary);
                    }
                }
            }
            self.ops
                .rename(&temp, &paths.binary)
                .map_err(|_| InstallError::Write {
                    path: paths.binary.clone(),
                    reason,
                })?;
        }

        Ok((confirmed, bytes))
    }

    /// Best-effort removal of images [`Installer::install`] moved aside on
    /// earlier upgrades. One still running refuses, and stays for the next
    /// install to try again.
    fn sweep_moved_aside(&self, paths: &RemotePaths) {
        let Ok(Some(entries)) = self.ops.list_dir(&paths.bin_dir) else {
            return;
        };
        for name in entries {
            if name.starts_with(".tty7-server-") && name.ends_with(MOVED_ASIDE_SUFFIX) {
                let _ = self.ops.remove_file(&format!("{}/{name}", paths.bin_dir));
            }
        }
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

    /// Whether this machine has never had a tty7 server on it — the question
    /// the consent prompt turns on.
    ///
    /// A launch leaves `tty7-server-c8p6.startup.log` and `.startup.exit`
    /// beside the binary, and both begin the same way it does. Counting one as
    /// a server would let someone who deleted the binary to uninstall tty7 get
    /// a new one downloaded and written without ever being asked.
    fn is_first_install(&self, paths: &RemotePaths) -> bool {
        let a_server = |name: &String| {
            name.starts_with("tty7-server-")
                && !name.ends_with(STARTUP_LOG_SUFFIX)
                && !name.ends_with(STARTUP_EXIT_SUFFIX)
        };
        match self.ops.list_dir(&paths.bin_dir) {
            Ok(Some(entries)) => !entries.iter().any(a_server),
            Ok(None) => true,
            Err(_) => true,
        }
    }

    fn ensure_daemon(
        &self,
        paths: &RemotePaths,
    ) -> Result<(bool, Option<MismatchedRemoteDaemon>), InstallError> {
        if self.daemon_is_serving(paths)? {
            return Ok((false, self.check_running_build(paths)));
        }

        let log = self.launch_daemon(paths)?;
        self.wait_for_launch(paths, &log)?;
        Ok((true, self.check_running_build(paths)))
    }

    /// Wait out a daemon this call just launched, and say what happened if it
    /// never answers.
    ///
    /// Two things used to be thrown away here, and between them they made every
    /// remote startup failure read the same: the probe's exit status and stderr
    /// (only `out.success()` survived) and the daemon's own output ([`launch_command`]
    /// sent it to `/dev/null`). All anyone ever saw was "nothing was answering
    /// after 15s", which names the symptom and not one cause.
    fn wait_for_launch(&self, paths: &RemotePaths, log: &StartupLog) -> Result<(), InstallError> {
        let deadline = Instant::now() + self.startup_timeout;
        loop {
            let probe = match self.control_probe(paths)? {
                None => return Ok(()),
                Some(reason) => reason,
            };
            // A process that has already *failed* will not start answering.
            // Ask before sleeping again: waiting out the full timeout for a
            // daemon that died in the first 200ms is fifteen seconds of
            // nothing.
            //
            // A clean exit is not that. `run_daemon` returns success when
            // another server already holds the single-server lock, so status 0
            // means "someone else is the server here" — which is good news
            // arriving early. Keep probing for that someone; only the deadline
            // ends this.
            let exit = log.exit_status(self.ops).filter(|status| status != "0");
            if exit.is_none() && Instant::now() < deadline {
                std::thread::sleep(self.poll_interval);
                continue;
            }
            return Err(InstallError::Launch {
                reason: log.explain(self.ops, &paths.binary, &probe, exit, self.startup_timeout),
            });
        }
    }

    /// `None` when the control socket answered, `Some(why)` when it did not.
    fn control_probe(&self, paths: &RemotePaths) -> Result<Option<String>, InstallError> {
        let cmd = match paths.platform {
            RemotePlatform::Unix => format!(
                "{} --stdio --bridge < /dev/null",
                shell_quote(&paths.binary)
            ),
            RemotePlatform::Windows => windows_host::control_probe_command(&paths.binary),
        };
        match self.ops.run(&cmd) {
            Ok(out) if out.success() => Ok(None),
            Ok(out) => Ok(Some(out.failure_reason())),
            Err(reason) => Err(InstallError::Launch { reason }),
        }
    }

    fn daemon_is_serving(&self, paths: &RemotePaths) -> Result<bool, InstallError> {
        Ok(self.control_probe(paths)?.is_none())
    }

    fn launch_daemon(&self, paths: &RemotePaths) -> Result<StartupLog, InstallError> {
        let log = StartupLog::for_binary(&paths.binary);
        let script = match paths.platform {
            RemotePlatform::Unix => {
                let settle = self.ops.launch_settle(&paths.binary);
                launch_script(&paths.binary, &log, settle)
            }
            RemotePlatform::Windows => {
                windows_host::launch_command(&paths.binary, &log.log, &log.exit, &log.nonce)
            }
        };
        self.ops
            .spawn_detached(&script)
            .map_err(|reason| InstallError::Launch { reason })?;
        Ok(log)
    }

    fn check_running_build(&self, paths: &RemotePaths) -> Option<MismatchedRemoteDaemon> {
        let exe = self.running_server_exe(paths.platform)?;
        let exe = exe.as_str();
        if asset::dialect_from_path(exe) == Some(self.dialect.dialect()) || exe == paths.binary {
            return None;
        }
        let spoken = self.probe_protocol(paths.platform, exe);
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

    /// Stop whatever tty7-server is running over there and start the one this
    /// build speaks to.
    ///
    /// Which is not the same binary: the path is named after *our* dialect
    /// (`tty7-server-c{control}p{protocol}`), while the kill matches any
    /// `tty7-server-*` — see [`TERMINATE_RUNNING_COMMAND`]. When
    /// the far end is a machine we have never installed onto — the other side
    /// of a dialect bump, most of all — the two are different files and the
    /// second one is not there. Restarting into it would end every session on
    /// the machine, including other clients', and then have nothing to launch.
    /// So look before killing: a machine with no matching server to start is
    /// left exactly as it was, and told to install one first.
    pub fn restart_daemon(&self) -> Result<(), InstallError> {
        let home = self.ops.home_dir().map_err(InstallError::NoHome)?;
        let paths = self.paths_for(RemotePlatform::of_sftp_home(&home), &home);
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
        install_progress().report(&self.host, InstallPhase::Restarting);

        // Keep why the stop failed, if it did. The command ends in `true`, so
        // anything short of success means the far end never reached the kill at
        // all — a login shell that choked on the script, a connection that went
        // away. Throwing that away is half of what made the no-`/proc` bug cost
        // a report instead of one glance at a log: the only thing anyone ever
        // saw was the timeout below, and it blames a daemon for not stopping
        // when nothing had asked it to.
        let stop = match paths.platform {
            RemotePlatform::Unix => TERMINATE_RUNNING_COMMAND.to_string(),
            RemotePlatform::Windows => windows_host::stop_command(&paths.binary),
        };
        let stop_failure = match self.ops.run(&stop) {
            Ok(out) if out.success() => None,
            Ok(out) => Some(out.failure_reason()),
            Err(reason) => Some(reason),
        };
        if let Some(reason) = &stop_failure {
            log::warn!(
                "remote {}: asking the running server to stop did not succeed: {reason}",
                self.host,
            );
        }

        let shutdown_timeout = self.shutdown_timeout;
        let deadline = Instant::now() + shutdown_timeout;
        while self.daemon_is_serving(paths)? {
            if Instant::now() >= deadline {
                return Err(InstallError::Launch {
                    reason: match &stop_failure {
                        Some(reason) => format!(
                            "the running remote daemon did not stop within {shutdown_timeout:?}, \
                             and the command asking it to stop failed: {reason}"
                        ),
                        None => format!(
                            "the running remote daemon did not stop within {shutdown_timeout:?}"
                        ),
                    },
                });
            }
            std::thread::sleep(self.poll_interval);
        }

        let log = self.launch_daemon(paths)?;
        self.wait_for_launch(paths, &log)
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
/// `tty7-server-c7p6` — which is why the fallback stays a fallback and `/proc`
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

/// Starts the line [`Installer::quick_prove`]'s script prints once the server
/// answered, so nothing a login script prints can pass for it.
const QUICK_PROVE_MARK: &str = "tty7-quick-prove ";

const TERMINATE_RUNNING_COMMAND: &str = r#"if [ -d /proc ]; then for p in /proc/[0-9]*; do e=$(readlink "$p/exe" 2>/dev/null) || continue; case "$e" in */tty7-server-*) kill -TERM "${p#/proc/}" 2>/dev/null; break;; esac; done; else ps -xwwo pid=,comm= 2>/dev/null | while read -r pid e; do case "$e" in */tty7-server-*) kill -TERM "$pid" 2>/dev/null; break;; esac; done; fi; true"#;

/// The two files a launch writes beside the server binary. Named here because
/// `is_first_install` reads the same directory and must not mistake one of
/// these for an installed server.
pub(crate) const STARTUP_LOG_SUFFIX: &str = ".startup.log";
pub(crate) const STARTUP_EXIT_SUFFIX: &str = ".startup.exit";

/// Where a launched daemon's output and exit status land on the far end.
///
/// One pair of files per binary, truncated at each launch rather than named
/// uniquely per attempt: these exist to be read seconds later by the client
/// that wrote them, and a unique name per launch would leave a file behind on
/// every reconnect for nobody to ever delete.
///
/// What makes the fixed name safe is the nonce. A restart tells the old daemon
/// to stop and the new one to start, and the old one's wrapper records its
/// status only once the old process is fully gone — which is after its socket
/// stopped answering, so it can land *after* the new launch truncated the file.
/// Reading that as the new daemon's status would fail a restart that is going
/// fine. The status is written with the nonce that asked for it, and a status
/// carrying anyone else's is not an answer to this launch.
pub(crate) struct StartupLog {
    log: String,
    exit: String,
    nonce: String,
}

/// How much of the far end's startup log to fetch.
const TAIL_BYTES: usize = 4096;

/// How much of it reaches the error string. The whole tail goes to this
/// client's log; a status strip gets the last few lines, which is where the
/// reason always is.
const TAIL_LINES: usize = 4;

impl StartupLog {
    pub(crate) fn for_binary(binary: &str) -> Self {
        Self {
            log: format!("{binary}{STARTUP_LOG_SUFFIX}"),
            exit: format!("{binary}{STARTUP_EXIT_SUFFIX}"),
            nonce: uuid::Uuid::new_v4().simple().to_string(),
        }
    }

    /// The exit status the wrapper recorded for *this* launch, or `None` while
    /// the daemon is still running. Only the wrapper writes this file, and only
    /// once it has waited for the daemon, so a status carrying this launch's
    /// nonce is the death certificate.
    fn exit_status(&self, ops: &dyn RemoteOps) -> Option<String> {
        let cmd = if asset::is_windows_sftp_path(&self.exit) {
            windows_host::read_exit_command(&self.exit)
        } else {
            format!("cat {} 2>/dev/null", shell_quote(&self.exit))
        };
        let out = ops.run(&cmd).ok()?;
        let (nonce, status) = out.stdout.trim().split_once(' ')?;
        (nonce == self.nonce && !status.is_empty()).then(|| status.to_string())
    }

    fn tail(&self, ops: &dyn RemoteOps) -> String {
        if asset::is_windows_sftp_path(&self.log) {
            let cmd = windows_host::tail_command(&self.log, TAIL_BYTES);
            return ops.run(&cmd).map(|out| out.stdout).unwrap_or_default();
        }
        let cmd = format!(
            "tail -c {TAIL_BYTES} {} 2>/dev/null",
            shell_quote(&self.log)
        );
        ops.run(&cmd).map(|out| out.stdout).unwrap_or_default()
    }

    /// Why the daemon never answered, in a sentence a status strip can show.
    ///
    /// The full tail goes to this client's log; the message keeps the last few
    /// lines of it, because the interesting one is always the last thing the
    /// server managed to say before it gave up.
    fn explain(
        &self,
        ops: &dyn RemoteOps,
        binary: &str,
        probe: &str,
        exit: Option<String>,
        waited: Duration,
    ) -> String {
        let tail = self.tail(ops);
        if !tail.trim().is_empty() {
            log::warn!("remote {binary} startup log ({}):\n{tail}", self.log);
        }
        let what = match &exit {
            Some(status) => format!("{binary} exited with status {status} before it answered"),
            None => format!("{binary} was still not answering after {waited:?}"),
        };
        let mut out = format!("{what} on the control socket");
        if !probe.trim().is_empty() {
            out.push_str(&format!("; last probe said: {}", probe.trim()));
        }
        let said: Vec<&str> = tail
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .rev()
            .take(TAIL_LINES)
            .collect();
        if said.is_empty() {
            out.push_str(&format!("; it logged nothing to {}", self.log));
        } else {
            let said: Vec<&str> = said.into_iter().rev().collect();
            out.push_str(&format!(
                "; it said: {} (full log at {})",
                said.join(" / "),
                self.log
            ));
        }
        out
    }
}

/// Start the daemon detached, keeping what it says and how it ends.
///
/// The output used to go to `/dev/null`, which is why a remote that refused to
/// come up could only ever be reported as silence. Everything the server
/// prints on the way up — the control socket it bound, the listener it could
/// not bind, the other server already holding the single-server lock — is a
/// `startup_note!` on stderr, and all of it was being discarded.
fn launch_command(binary: &str, log: &StartupLog) -> String {
    let bin = shell_quote(binary);
    let log_path = shell_quote(&log.log);
    let exit_path = shell_quote(&log.exit);
    let nonce = &log.nonce;
    // The wrapper outlives the daemon by one line: it waits, then records the
    // status, stamped with the nonce that asked for it.
    let supervised = shell_quote(&format!(
        "{bin} --daemon; s=$?; printf '%s %s' {nonce} \"$s\" > {exit_path}; exit \"$s\""
    ));
    // Three things this preamble has to get right, each of them a way to break
    // a machine that used to work:
    //
    // - The `umask` is scoped to the subshell. Left bare it would apply to the
    //   whole login shell, so the daemon would inherit it and so would every
    //   pane shell it forks — and a `git clone` in a remote pane would produce
    //   files nobody but the owner can read.
    // - Both files are removed and re-created here, under that umask, and only
    //   truncated by the wrapper. `>` on an existing file keeps whatever mode
    //   that file already had, so truncating alone would let one left at 0644 —
    //   by an older build, by anything — stay that way forever.
    // - A home that is full, read-only, or not ours must not stop the daemon
    //   from starting. If the files cannot be made, the redirect falls back to
    //   `/dev/null` and the launch goes ahead without diagnostics, which is
    //   exactly what it did before. The whole attempt sits inside a subshell so
    //   that a redirection failure cannot take the login shell down with it —
    //   on a POSIX shell a redirection error on the special builtin `:` ends a
    //   non-interactive shell outright, and `|| true` never gets to run.
    format!(
        "out={log_path}; \
         (umask 077; rm -f {log_path} {exit_path}; : > {log_path}; : > {exit_path}) 2>/dev/null \
           || out=/dev/null; \
         if command -v setsid >/dev/null 2>&1; then \
           setsid sh -c {supervised} < /dev/null >> \"$out\" 2>&1 & \
         else \
           nohup sh -c {supervised} < /dev/null >> \"$out\" 2>&1 & \
         fi"
    )
}

fn launch_script(binary: &str, log: &StartupLog, settle: Option<String>) -> String {
    let launch = launch_command(binary, log);
    match settle {
        Some(settle) => format!("{launch}\n{settle}"),
        None => launch,
    }
}

fn unique_temp(shared: &str) -> String {
    let pid = std::process::id();
    if let Some(stem) = shared.strip_suffix(".tmp.exe") {
        return format!("{stem}.{pid}.tmp.exe");
    }
    match shared.strip_suffix(".tmp") {
        Some(stem) => format!("{stem}.{pid}.tmp"),
        None => format!("{shared}.{pid}"),
    }
}

/// Whether a file SFTP describes can be run as the server. Unix wants the
/// owner's execute bit; Windows has none to want — an `.exe` runs because of
/// its name, and SFTP's mode bits there are a translation of ACLs that says
/// nothing reliable about it.
fn runnable(platform: RemotePlatform, stat: RemoteStat) -> bool {
    !stat.is_dir
        && match platform {
            RemotePlatform::Unix => stat.mode & 0o100 != 0,
            RemotePlatform::Windows => true,
        }
}

const MOVED_ASIDE_SUFFIX: &str = ".old";

/// Where a running Windows image is renamed to make room for its upgrade: a
/// hidden sibling, so [`Installer::is_first_install`] never counts it, and
/// unique per attempt: an image still running from an earlier move-aside
/// holds its name, and a second upgrade must not need it.
fn moved_aside(binary: &str) -> String {
    let (dir, name) = binary.rsplit_once('/').unwrap_or(("", binary));
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{dir}/.{name}.{}{MOVED_ASIDE_SUFFIX}", &id[..12])
}

/// The command a routed link runs on the remote to reach its server:
/// `'<binary>' --stdio` for a POSIX shell, `"<binary>" --stdio` for the
/// `cmd.exe` Windows OpenSSH runs commands through. See
/// [`windows_host::stdio_command`] for why not PowerShell.
pub fn server_stdio_command(binary: &str) -> String {
    if asset::is_windows_sftp_path(binary) {
        windows_host::stdio_command(binary)
    } else {
        format!("{} --stdio", shell_quote(binary))
    }
}

pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn connection_label(conn: &SshConnection) -> String {
    conn.key().as_str().to_string()
}

/// What one SSH connection's server probe proved, kept so that the panes after
/// the first one do not pay for proving it again.
///
/// `Installer::run` is four to six serial round trips — `uname -sm`, an SFTP
/// realpath for the home directory, an SFTP stat, a control probe that spawns
/// the server binary, and `check_running_build`, which walks `/proc/<pid>/exe`
/// with a `readlink` per PID (or shells out to `ps` where there is no `/proc`).
/// That is a fair price once for a machine and an absurd one per pane: issue
/// #695 is a user watching `connecting to ...` for ten seconds every time they
/// open a tab on a host tty7 was already connected to and already serving. The
/// SSH connection itself is reused, so none of that wait is handshake cost.
///
/// Deliberately not a global map keyed by host, the way [`wsl`]'s is. A distro
/// name is the whole identity of a WSL target, but an SSH connection can die
/// and be replaced under the same key, and a note about the previous link must
/// not answer for the next one. Keying by connection generation *is* keeping
/// the note on the connection — see `SshConnection::proved_server`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvedServer {
    /// The server binary the probe settled on, which is what the route runs.
    pub binary: String,
    /// The build mismatch the probe found, if it found one.
    ///
    /// Kept because the warning is raised inside `Installer::run`, and the
    /// whole point of the memo is that `run` does not happen again: without
    /// this, only the first pane on a connection would ever hear that a
    /// different build is serving the machine, and every pane after it — every
    /// window, since a connection outlives one — would attach in silence. That
    /// is the one way memoizing could quietly cancel the version check, so it
    /// is the one thing the note carries besides the path.
    pub mismatch: Option<MismatchedRemoteDaemon>,
}

impl ProvedServer {
    fn from_report(report: InstallReport) -> ProvedServer {
        ProvedServer {
            binary: report.paths.binary,
            mismatch: report.mismatch,
        }
    }
}

/// Answer from the note if there is one, otherwise prove it and leave a note.
///
/// Two rules the callers depend on:
///
/// - A failed probe is not remembered. `prove` returning an error leaves the
///   slot exactly as it found it, so a host that was briefly unreachable, or an
///   install the user declined once, is retried by the next pane rather than
///   pinned into permanent failure for the life of the connection.
/// - A remembered mismatch is re-filed on every hit. Each route carries its own
///   mismatch sink (`RouteSetup::mismatches`), drained into a prompt as the
///   route is set up, so re-filing is what makes the *n*th pane's client hear
///   what the first pane's probe found.
fn proved_or_prove(
    slot: &mut Option<ProvedServer>,
    prove: impl FnOnce() -> io::Result<ProvedServer>,
) -> io::Result<String> {
    if let Some(known) = slot.as_ref() {
        if let Some(mismatch) = known.mismatch.clone() {
            record_remote_mismatches(vec![mismatch]);
        }
        return Ok(known.binary.clone());
    }
    let proved = prove()?;
    let binary = proved.binary.clone();
    *slot = Some(proved);
    Ok(binary)
}

pub fn ensure_remote_server(conn: &Arc<SshConnection>) -> io::Result<String> {
    let host = connection_label(conn);
    ensure_remote_server_labeled(conn, &host)
}

pub fn ensure_remote_server_labeled(conn: &Arc<SshConnection>, host: &str) -> io::Result<String> {
    // The lock is held across the probe, not just across the read: two panes
    // opening at once on a cold connection would otherwise both install, and
    // the loser would be uploading over the very file the winner is renaming
    // into place. Waiting out an install is what the second pane wants to do
    // anyway — it needs the same answer.
    let mut slot = conn.proved_server();
    if let Some(known) = slot.as_ref() {
        log::debug!(
            "remote {host}: tty7-server was already proved at {} on this connection",
            known.binary,
        );
    }
    proved_or_prove(&mut slot, || {
        let ops = ssh_ops::SshRemoteOps::new(conn.clone());
        let fetch = default_fetcher();
        let confirm = install_confirm();
        let source = BundledOrRelease::discover(fetch.as_ref());
        let installer = Installer::with_source(&ops, &source, confirm.as_ref(), host);
        if let Some(proved) = installer.quick_prove() {
            log::info!(
                "remote {host}: tty7-server already serving at {}",
                proved.binary
            );
            return Ok(proved);
        }
        let report = installer.run()?;
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
        Ok(ProvedServer::from_report(report))
    })
}

/// Drop what we thought we knew about a connection's server, so that the next
/// `ensure_remote_server` on it proves the whole thing again.
///
/// The router's call, and between it and [`while_changing_the_server`] they are
/// the memo's correctness argument: this one is made when a routed link closes
/// without the remote ever sending a byte — the only way a path that has stopped
/// working is found out — and that one covers [`restart_remote_daemon`] and
/// [`replace_remote_server`], which change which build is serving the machine
/// and therefore what the note claims. A reconnect needs no caller at all: the
/// note lives on the connection, and a new connection has none.
///
/// Takes the lock and gives it straight back, so it is safe to call from the
/// reactor — unlike anything that holds it across a probe or a replace.
pub fn forget_remote_server(conn: &SshConnection) {
    *conn.proved_server() = None;
}

/// Run something that changes which server is running over there, with the note
/// dropped first and its lock held for the whole operation.
///
/// Forget first, not afterwards: both callers deliberately change what is
/// running over there, which is most of what the note claims, and one that
/// fails halfway must leave the next pane looking rather than trusting a note
/// written before the upheaval.
///
/// Held throughout for the same reason `ensure_remote_server` holds it across
/// its probe. A pane opening while a replace is in flight would otherwise probe
/// against a half-moved binary — installing over the very upload this call is
/// renaming into place, or filing the outgoing daemon as the mismatch — and
/// then *keep* that answer for the life of the connection, where before the
/// memo it cost that one pane and no other. Waiting the replace out is what the
/// pane wants anyway: the answer it is asking for is the one this call is in
/// the business of changing.
///
/// The note is cleared inside the guard rather than by calling
/// [`forget_remote_server`], which wants this same non-reentrant lock.
fn while_changing_the_server(
    conn: &SshConnection,
    change: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let mut slot = conn.proved_server();
    *slot = None;
    change()
}

pub fn restart_remote_daemon(conn: &Arc<SshConnection>) -> io::Result<()> {
    let host = connection_label(conn);
    let ops = ssh_ops::SshRemoteOps::new(conn.clone());
    let fetch = default_fetcher();
    let confirm = install_confirm();
    while_changing_the_server(conn, || {
        Installer::new(&ops, fetch.as_ref(), confirm.as_ref(), host).restart_daemon()?;
        Ok(())
    })
}

pub fn replace_remote_server(conn: &Arc<SshConnection>) -> io::Result<()> {
    put_server_on_remote(conn, false)
}

/// Install this build's server over the one already there, even when it speaks
/// our dialect, and restart it. See [`Installer::replace_forced`].
pub fn update_remote_server(conn: &Arc<SshConnection>) -> io::Result<()> {
    put_server_on_remote(conn, true)
}

fn put_server_on_remote(conn: &Arc<SshConnection>, force: bool) -> io::Result<()> {
    let host = connection_label(conn);
    let ops = ssh_ops::SshRemoteOps::new(conn.clone());
    let fetch = default_fetcher();
    let confirm = install_confirm();
    let source = BundledOrRelease::discover(fetch.as_ref());
    while_changing_the_server(conn, || {
        let installer = Installer::with_source(&ops, &source, confirm.as_ref(), host);
        if force {
            installer.replace_forced()?;
        } else {
            installer.replace()?;
        }
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

#[cfg(test)]
mod windows_tests;

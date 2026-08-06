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

    fn paths_for(&self, home: &str) -> RemotePaths {
        asset::remote_paths(home, self.dialect.control, self.dialect.protocol)
    }

    pub fn replace(&self) -> Result<(), InstallError> {
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

        self.restart_daemon()
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

    pub fn run(&self) -> Result<InstallReport, InstallError> {
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
        let _ = self.ops.chmod(&paths.bin_dir, DIR_MODE);

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

        self.ops
            .chmod(&temp, BINARY_MODE)
            .map_err(|reason| InstallError::Write {
                path: temp.clone(),
                reason,
            })?;

        let spoke = self.probe_protocol(&temp);
        if !spoke.as_ref().is_some_and(|s| s.serves(&self.dialect)) {
            let _ = self.ops.remove_file(&temp);
            return Err(InstallError::DialectMismatch {
                origin: asset_origin,
                wanted: self.dialect.clone(),
                spoke,
            });
        }

        if let Err(reason) = self.ops.rename(&temp, &paths.binary) {
            let _ = self.ops.remove_file(&paths.binary);
            self.ops
                .rename(&temp, &paths.binary)
                .map_err(|_| InstallError::Write {
                    path: paths.binary.clone(),
                    reason,
                })?;
        }

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

    pub fn restart_daemon(&self) -> Result<(), InstallError> {
        let home = self.ops.home_dir().map_err(InstallError::NoHome)?;
        let paths = self.paths_for(&home);
        install_progress().report(&self.host, InstallPhase::Restarting);

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

const RUNNING_EXE_COMMAND: &str = r#"for p in /proc/[0-9]*; do e=$(readlink "$p/exe" 2>/dev/null) || continue; case "$e" in */tty7-server-*) printf '%s' "${e% (deleted)}"; break;; esac; done; true"#;

const TERMINATE_RUNNING_COMMAND: &str = r#"for p in /proc/[0-9]*; do e=$(readlink "$p/exe" 2>/dev/null) || continue; case "$e" in */tty7-server-*) kill -TERM "${p#/proc/}" 2>/dev/null; break;; esac; done; true"#;

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

fn launch_script(binary: &str, settle: Option<String>) -> String {
    let launch = launch_command(binary);
    match settle {
        Some(settle) => format!("{launch}\n{settle}"),
        None => launch,
    }
}

fn unique_temp(shared: &str) -> String {
    let pid = std::process::id();
    match shared.strip_suffix(".tmp") {
        Some(stem) => format!("{stem}.{pid}.tmp"),
        None => format!("{shared}.{pid}"),
    }
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
    let ops = ssh_ops::SshRemoteOps::new(conn.clone());
    let fetch = default_fetcher();
    let confirm = install_confirm();
    let source = BundledOrRelease::discover(fetch.as_ref());
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

pub fn restart_remote_daemon(conn: &Arc<SshConnection>) -> io::Result<()> {
    let host = connection_label(conn);
    let ops = ssh_ops::SshRemoteOps::new(conn.clone());
    let fetch = default_fetcher();
    let confirm = install_confirm();
    Installer::new(&ops, fetch.as_ref(), confirm.as_ref(), host).restart_daemon()?;
    Ok(())
}

pub fn replace_remote_server(conn: &Arc<SshConnection>) -> io::Result<()> {
    let host = connection_label(conn);
    let ops = ssh_ops::SshRemoteOps::new(conn.clone());
    let fetch = default_fetcher();
    let confirm = install_confirm();
    let source = BundledOrRelease::discover(fetch.as_ref());
    Installer::with_source(&ops, &source, confirm.as_ref(), host).replace()?;
    Ok(())
}

#[cfg(feature = "remote-install")]
fn default_fetcher() -> Arc<dyn AssetFetcher> {
    Arc::new(download::HttpsFetcher::default())
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

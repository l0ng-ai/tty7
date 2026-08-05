use anyhow::{Context as _, Result};
use gpui::http_client::{AsyncBody, HttpClient as _, HttpRequestExt as _, RedirectPolicy};
use gpui::{AnyWindowHandle, App, AsyncApp, Global, PromptLevel, Window, http_client};
use reqwest_client::ReqwestClient;
use smol::future::FutureExt as _;
use smol::io::AsyncReadExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use tty7_core::daemon::install::AssetFetcher as _;

use crate::core::config::Config;

const REPO: &str = "l0ng-ai/tty7";

pub const RELEASES_URL: &str = "https://github.com/l0ng-ai/tty7/releases/latest";

const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(target_os = "windows")]
const WINDOWS_INNO_INSTALL_MARKER: &str = ".tty7-inno-install";
#[cfg(target_os = "windows")]
const WINDOWS_PORTABLE_MARKER: &str = ".tty7-portable";
#[cfg(target_os = "windows")]
const WINDOWS_PORTABLE_MARKER_CONTENT: &[u8] = b"portable-v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub version: String,
    pub installable: bool,
    pub install_hint: Option<UpdateInstallHint>,
    asset: Option<ReleaseAsset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateInstallHint {
    #[cfg(target_os = "macos")]
    UnsupportedMacos,
    #[cfg(target_os = "linux")]
    UnsupportedLinux,
    #[cfg(target_os = "windows")]
    UnsupportedWindows,
    #[cfg(target_os = "windows")]
    WindowsAllUsersInstall,
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    UnsupportedPlatform,
    MissingPackage(String),
    MissingChecksums,
}

impl UpdateInstallHint {
    fn english(&self) -> String {
        match self {
            #[cfg(target_os = "macos")]
            Self::UnsupportedMacos => "This copy is not running from a writable tty7.app bundle, so replacing it would be unsafe. Move tty7 to Applications or another writable folder, or open the release page to install the update.".to_string(),
            #[cfg(target_os = "linux")]
            Self::UnsupportedLinux => "The first in-app updater supports packaged macOS app bundles. Use the release page or your package manager to update this Linux installation.".to_string(),
            #[cfg(target_os = "windows")]
            Self::UnsupportedWindows => "Automatic Windows updates are available for recognized Inno Setup and portable ZIP installations. This copy is missing a valid installation marker, updater, or writable portable directory, so open the release page to update it manually.".to_string(),
            #[cfg(target_os = "windows")]
            Self::WindowsAllUsersInstall => "tty7 is installed for all users, which needs administrator rights to replace. tty7 will not raise an elevation prompt on its own behalf, so open the release page and run the installer yourself to update it.".to_string(),
            #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
            Self::UnsupportedPlatform => "Automatic installation is not available on this platform. Open the release page.".to_string(),
            Self::MissingPackage(name) => format!(
                "The release has no {name} package for this installation. Open the release page to choose another package."
            ),
            Self::MissingChecksums => "The release has no checksums.txt, so tty7 refuses to install it automatically.".to_string(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Downloading,
    Installing,
    Failed(UpdateFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateFailure {
    Check(String),
    Prepare(String),
    Launch(String),
}

#[derive(Clone, Debug, Default)]
pub struct UpdateStatus {
    pub available: Option<AvailableUpdate>,
    pub phase: UpdatePhase,
}

impl Global for UpdateStatus {}

pub fn spawn_check(cx: &mut App) {
    if !cx.global::<Config>().check_for_updates {
        return;
    }
    spawn_check_inner(false, cx);
}

pub fn spawn_check_forced(cx: &mut App) {
    spawn_check_inner(true, cx);
}

fn spawn_check_inner(report_failure: bool, cx: &mut App) {
    if cx.try_global::<UpdateStatus>().is_some_and(|status| {
        matches!(
            status.phase,
            UpdatePhase::Checking | UpdatePhase::Downloading | UpdatePhase::Installing
        )
    }) {
        return;
    }
    let previous_available = cx
        .try_global::<UpdateStatus>()
        .and_then(|status| status.available.clone());
    set_status(
        UpdateStatus {
            available: previous_available.clone(),
            phase: UpdatePhase::Checking,
        },
        cx,
    );

    cx.spawn(async move |cx| {
        let current = env!("CARGO_PKG_VERSION");
        let release = match fetch_latest_release()
            .or(async {
                cx.background_executor().timer(CHECK_TIMEOUT).await;
                Err(anyhow::anyhow!("timed out after {CHECK_TIMEOUT:?}"))
            })
            .await
        {
            Ok(v) => v,
            Err(e) => {
                log::debug!("update check skipped: {e:#}");
                if report_failure {
                    let detail = format!("{e:#}");
                    cx.update(|cx| {
                        set_status(
                            UpdateStatus {
                                available: previous_available,
                                phase: UpdatePhase::Failed(UpdateFailure::Check(detail)),
                            },
                            cx,
                        )
                    });
                } else {
                    cx.update(|cx| set_status(UpdateStatus::default(), cx));
                }
                return;
            }
        };

        if !is_update_available(&release.tag_name, current) {
            log::debug!(
                "update check: up to date (latest {}, running {current})",
                release.tag_name
            );
            cx.update(|cx| {
                set_status(
                    UpdateStatus {
                        available: None,
                        phase: UpdatePhase::UpToDate,
                    },
                    cx,
                )
            });
            return;
        }

        let version = release.tag_name.trim_start_matches('v').to_string();
        let selection = select_release_asset(&version, &release.assets);
        let available = AvailableUpdate {
            version: version.clone(),
            installable: selection.asset.is_some(),
            install_hint: selection.reason,
            asset: selection.asset,
        };
        log::info!("update available: {version} (running {current})");

        cx.update(|cx| {
            set_status(
                UpdateStatus {
                    available: Some(available.clone()),
                    phase: UpdatePhase::Idle,
                },
                cx,
            )
        });

        if UpdateState::load().last_prompted.as_deref() == Some(version.as_str()) {
            return;
        }

        let Some(window) = wait_for_window(cx).await else {
            return;
        };
        let shown = cx.update(|cx| {
            window
                .update(cx, |_root, window, cx| {
                    prompt_update(&available, window, cx)
                })
                .is_ok()
        });

        if shown {
            UpdateState {
                last_prompted: Some(version),
            }
            .save();
        }
    })
    .detach();
}

fn set_status(status: UpdateStatus, cx: &mut App) {
    cx.set_global(status);
    cx.refresh_windows();
}

async fn wait_for_window(cx: &mut AsyncApp) -> Option<AnyWindowHandle> {
    for _ in 0..50 {
        if let Some(handle) = cx.update(|cx| cx.windows().first().copied()) {
            return Some(handle);
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
    None
}

fn prompt_update(update: &AvailableUpdate, window: &mut Window, cx: &mut App) {
    let install_hint = update.install_hint.as_ref().map(UpdateInstallHint::english);
    let detail = if update.installable {
        let note = install_hint
            .as_deref()
            .map(|note| format!(" {note}"))
            .unwrap_or_default();
        format!(
            "tty7 {} is available — you're on {}. tty7 can download the verified update, install \
             it, and restart the app.{note}",
            update.version,
            env!("CARGO_PKG_VERSION")
        )
    } else {
        format!(
            "tty7 {} is available — you're on {}. {}",
            update.version,
            env!("CARGO_PKG_VERSION"),
            install_hint
                .as_deref()
                .unwrap_or("This installation cannot update itself.")
        )
    };
    let action = if update.installable {
        "Update and Relaunch"
    } else {
        "View Release"
    };
    let answer = window.prompt(
        PromptLevel::Info,
        "Update available",
        Some(&detail),
        &["Later", action],
        cx,
    );
    let update = update.clone();
    cx.spawn(async move |cx| {
        if let Ok(1) = answer.await {
            if update.installable {
                let _ = cx.update(|cx| install(update, cx));
            } else {
                open_releases_page();
            }
        }
    })
    .detach();
}

pub fn install_available(cx: &mut App) {
    let Some(update) = cx
        .try_global::<UpdateStatus>()
        .and_then(|status| status.available.clone())
    else {
        return;
    };
    if update.installable {
        install(update, cx);
    } else {
        open_releases_page();
    }
}

fn install(update: AvailableUpdate, cx: &mut App) {
    if cx.try_global::<UpdateStatus>().is_some_and(|status| {
        matches!(
            status.phase,
            UpdatePhase::Downloading | UpdatePhase::Installing
        )
    }) {
        return;
    }
    let Some(asset) = update.asset.clone() else {
        open_releases_page();
        return;
    };
    set_status(
        UpdateStatus {
            available: Some(update.clone()),
            phase: UpdatePhase::Downloading,
        },
        cx,
    );

    let version = update.version.clone();
    let task = cx
        .background_executor()
        .spawn(smol::unblock(move || prepare_update(&version, &asset)));
    cx.spawn(async move |cx| {
        let prepared = match task.await {
            Ok(prepared) => prepared,
            Err(error) => {
                let detail = format!("{error:#}");
                log::error!("update failed: {detail}");
                cx.update(|cx| {
                    set_status(
                        UpdateStatus {
                            available: Some(update),
                            phase: UpdatePhase::Failed(UpdateFailure::Prepare(detail)),
                        },
                        cx,
                    )
                });
                return;
            }
        };

        cx.update(|cx| {
            set_status(
                UpdateStatus {
                    available: Some(update.clone()),
                    phase: UpdatePhase::Installing,
                },
                cx,
            )
        });
        match prepared.launch() {
            Ok(()) => {
                let _ = cx.update(|cx| cx.quit());
            }
            Err(error) => {
                let detail = format!("{error:#}");
                log::error!("could not start the installer: {detail}");
                cx.update(|cx| {
                    set_status(
                        UpdateStatus {
                            available: Some(update),
                            phase: UpdatePhase::Failed(UpdateFailure::Launch(detail)),
                        },
                        cx,
                    )
                });
            }
        }
    })
    .detach();
}

pub fn open_releases_page() {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener).arg(RELEASES_URL).spawn() {
        log::warn!("failed to open releases page: {e}");
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct UpdateState {
    #[serde(default)]
    last_prompted: Option<String>,
}

impl UpdateState {
    fn path() -> Option<std::path::PathBuf> {
        crate::core::config::config_path("update.json")
    }

    fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            log::warn!("failed to parse {}: {e}; ignoring", path.display());
            Self::default()
        })
    }

    fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("failed to serialize update state: {e}");
                return;
            }
        };
        if let Err(e) = crate::core::config::write_atomic(&path, json.as_bytes()) {
            log::warn!("failed to write {}: {e}", path.display());
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
struct LatestRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Clone, Debug, serde::Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

async fn fetch_latest_release() -> Result<LatestRelease> {
    let client = ReqwestClient::user_agent(concat!("tty7/", env!("CARGO_PKG_VERSION")))
        .context("building HTTP client")?;

    // `/releases/latest` intentionally excludes prereleases, so Nightly builds
    // are offered the Stable release that supersedes them and no rolling
    // prerelease can ever become an update source.
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let request = http_client::Request::get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .follow_redirects(RedirectPolicy::FollowAll)
        .body(AsyncBody::default())
        .context("building request")?;

    let mut response = client
        .send(request)
        .await
        .context("requesting latest release")?;

    if !response.status().is_success() {
        anyhow::bail!("GitHub API returned HTTP {}", response.status().as_u16());
    }

    let mut body = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut body)
        .await
        .context("reading response body")?;

    let release: LatestRelease = serde_json::from_slice(&body).context("parsing release JSON")?;
    Ok(release)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReleaseAsset {
    name: String,
    url: String,
    checksums_url: String,
}

struct AssetSelection {
    asset: Option<ReleaseAsset>,
    reason: Option<UpdateInstallHint>,
}

fn select_release_asset(version: &str, assets: &[GitHubAsset]) -> AssetSelection {
    select_release_asset_for(package_for_current_install(version), assets)
}

fn select_release_asset_for(
    package: Result<String, UpdateInstallHint>,
    assets: &[GitHubAsset],
) -> AssetSelection {
    let name = match package {
        Ok(name) => name,
        Err(reason) => {
            return AssetSelection {
                asset: None,
                reason: Some(reason),
            };
        }
    };
    let Some(asset) = assets.iter().find(|asset| asset.name == name) else {
        return AssetSelection {
            asset: None,
            reason: Some(UpdateInstallHint::MissingPackage(name)),
        };
    };
    let Some(checksums) = assets.iter().find(|asset| asset.name == "checksums.txt") else {
        return AssetSelection {
            asset: None,
            reason: Some(UpdateInstallHint::MissingChecksums),
        };
    };
    AssetSelection {
        asset: Some(ReleaseAsset {
            name,
            url: asset.browser_download_url.clone(),
            checksums_url: checksums.browser_download_url.clone(),
        }),
        reason: None,
    }
}

/// The release package this installation can replace itself with, or the
/// reason it cannot.
fn package_for_current_install(version: &str) -> Result<String, UpdateInstallHint> {
    #[cfg(target_os = "macos")]
    {
        let Some(app) = current_macos_app_bundle() else {
            return Err(UpdateInstallHint::UnsupportedMacos);
        };
        if !is_macos_update_writable(&app) || bundled_updater().is_none() {
            return Err(UpdateInstallHint::UnsupportedMacos);
        }
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else {
            return Err(UpdateInstallHint::UnsupportedMacos);
        };
        return Ok(format!("tty7-{version}-macos-{arch}.zip"));
    }
    #[cfg(target_os = "linux")]
    {
        let _ = version;
        return Err(UpdateInstallHint::UnsupportedLinux);
    }
    #[cfg(target_os = "windows")]
    {
        let Some(layout) = current_windows_update_layout() else {
            return Err(UpdateInstallHint::UnsupportedWindows);
        };
        windows_layout_is_updatable(&layout)?;
        if !layout.directory().join("tty7-updater.exe").is_file() {
            return Err(UpdateInstallHint::UnsupportedWindows);
        }
        return windows_package_for_layout(version, &layout)
            .ok_or(UpdateInstallHint::UnsupportedWindows);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = version;
        Err(UpdateInstallHint::UnsupportedPlatform)
    }
}

fn prepare_update(version: &str, asset: &ReleaseAsset) -> Result<PreparedUpdate> {
    let fetcher = tty7_core::daemon::install::download::HttpsFetcher::default();
    let checksums = fetcher
        .get(&asset.checksums_url)
        .map_err(anyhow::Error::msg)
        .context("downloading checksums.txt")?;
    let archive = fetcher
        .get(&asset.url)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("downloading {}", asset.name))?;
    #[cfg(target_os = "macos")]
    {
        return prepare_macos_update(version, &asset.name, &archive, &checksums);
    }
    #[cfg(target_os = "windows")]
    {
        return prepare_windows_update(version, &asset.name, &archive, &checksums);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    anyhow::bail!("automatic installation is not supported on this platform")
}

#[derive(Debug)]
struct PreparedUpdate {
    updater: PathBuf,
    args: Vec<PathBuf>,
    config_dir: Option<PathBuf>,
    stage: PathBuf,
}

impl PreparedUpdate {
    fn launch(self) -> Result<()> {
        let stage = self.stage;
        let mut command = Command::new(self.updater);
        command.args(self.args);
        if let Some(config_dir) = self.config_dir {
            command.env("TTY7_CONFIG_DIR", config_dir);
        }
        tty7_core::core::proc::hide_console(&mut command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&stage);
            })
            .context("launching tty7-updater")?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn update_staging_dir(parent: &Path) -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(".tty7-update-")
        .tempdir_in(parent)
        .context("creating update staging directory")
}

#[cfg(target_os = "windows")]
fn system_update_staging_dir() -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("tty7-update-")
        .tempdir()
        .context("creating the Windows update staging directory")
}

fn write_staged_asset(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let path = dir.join(name);
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(target_os = "macos")]
fn prepare_macos_update(
    version: &str,
    asset_name: &str,
    archive: &[u8],
    checksums: &[u8],
) -> Result<PreparedUpdate> {
    let current =
        current_macos_app_bundle().context("tty7 is not running from an application bundle")?;
    let parent = current
        .parent()
        .context("tty7.app has no parent directory")?;
    let updater = bundled_updater().context("tty7-updater is not bundled with this app")?;
    let staging = update_staging_dir(parent)?;
    let dir = staging.path().to_path_buf();
    let archive = write_staged_asset(&dir, asset_name, archive)?;
    let checksums = write_staged_asset(&dir, "checksums.txt", checksums)?;
    run_updater(
        &updater,
        [
            PathBuf::from("verify"),
            current.clone(),
            archive.clone(),
            checksums.clone(),
            PathBuf::from(asset_name),
            dir.clone(),
            PathBuf::from(version),
        ],
    )?;
    let log =
        crate::core::config::config_path("update.log").unwrap_or_else(|| dir.join("update.log"));
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).context("creating the update log directory")?;
    }
    let dir = staging.keep();
    Ok(PreparedUpdate {
        updater,
        args: vec![
            PathBuf::from("install"),
            std::process::id().to_string().into(),
            current,
            archive,
            checksums,
            PathBuf::from(asset_name),
            dir.clone(),
            PathBuf::from(version),
            log,
        ],
        config_dir: crate::core::config::config_dir_path(),
        stage: dir,
    })
}

#[cfg(target_os = "windows")]
fn prepare_windows_update(
    version: &str,
    asset_name: &str,
    package: &[u8],
    checksums: &[u8],
) -> Result<PreparedUpdate> {
    let layout = current_windows_update_layout()
        .context("tty7 is not running from a recognized Windows installation")?;
    // Re-checked here rather than trusting the check that produced the offer:
    // an installation can be relocated, or its privileges changed, between the
    // update check and the user pressing the button.
    if let Err(hint) = windows_layout_is_updatable(&layout) {
        anyhow::bail!("{}", hint.english());
    }
    let install_dir = layout.directory().to_path_buf();
    let bundled = bundled_updater().context("tty7-updater.exe is not bundled with this app")?;
    let staging = system_update_staging_dir()?;
    let dir = staging.path().to_path_buf();
    let package = write_staged_asset(&dir, asset_name, package)?;
    let checksums = write_staged_asset(&dir, "checksums.txt", checksums)?;

    // Verification runs before the GUI commits to quitting. Both Windows
    // update modes repeat their archive checks after the parent exits.
    let install_command = match &layout {
        WindowsUpdateLayout::Inno(_) => {
            run_updater(
                &bundled,
                [
                    PathBuf::from("verify"),
                    package.clone(),
                    checksums.clone(),
                    PathBuf::from(asset_name),
                    PathBuf::from(version),
                ],
            )?;
            "install"
        }
        WindowsUpdateLayout::Portable(_) => {
            run_updater(
                &bundled,
                [
                    PathBuf::from("verify-portable"),
                    package.clone(),
                    checksums.clone(),
                    PathBuf::from(asset_name),
                    PathBuf::from(version),
                    dir.clone(),
                ],
            )?;
            "install-portable"
        }
    };

    // Windows locks a running executable. Run a private copy from the staging
    // directory so Inno can replace the bundled helper in the installation.
    let updater = dir.join("tty7-updater.exe");
    std::fs::copy(&bundled, &updater)
        .with_context(|| format!("copying the Windows updater to {}", updater.display()))?;

    let log =
        crate::core::config::config_path("update.log").unwrap_or_else(|| dir.join("update.log"));
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).context("creating the update log directory")?;
    }
    let dir = staging.keep();
    Ok(PreparedUpdate {
        updater,
        args: vec![
            PathBuf::from(install_command),
            std::process::id().to_string().into(),
            package,
            checksums,
            PathBuf::from(asset_name),
            install_dir,
            PathBuf::from(version),
            log,
            dir.clone(),
        ],
        config_dir: crate::core::config::config_dir_path(),
        stage: dir,
    })
}

#[cfg(target_os = "macos")]
fn current_macos_app_bundle() -> Option<PathBuf> {
    std::env::current_exe().ok()?.ancestors().find_map(|path| {
        (path.extension().and_then(|ext| ext.to_str()) == Some("app")).then(|| path.to_path_buf())
    })
}

#[cfg(target_os = "macos")]
fn is_macos_update_writable(app: &Path) -> bool {
    app.parent().is_some_and(can_stage_replacement_in)
}

#[cfg(target_os = "macos")]
fn bundled_updater() -> Option<PathBuf> {
    let updater = current_macos_app_bundle()?.join("Contents/MacOS/tty7-updater");
    updater.is_file().then_some(updater)
}

#[cfg(target_os = "windows")]
fn bundled_updater() -> Option<PathBuf> {
    let updater = current_windows_update_layout()?
        .directory()
        .join("tty7-updater.exe");
    updater.is_file().then_some(updater)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn bundled_updater() -> Option<PathBuf> {
    None
}

#[cfg(target_os = "windows")]
#[derive(Clone, Debug, PartialEq, Eq)]
enum WindowsUpdateLayout {
    Inno(PathBuf),
    Portable(PathBuf),
}

#[cfg(target_os = "windows")]
impl WindowsUpdateLayout {
    fn directory(&self) -> &Path {
        match self {
            Self::Inno(directory) | Self::Portable(directory) => directory,
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_package_for_layout(version: &str, layout: &WindowsUpdateLayout) -> Option<String> {
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        return None;
    };
    Some(match layout {
        WindowsUpdateLayout::Inno(_) => format!("tty7-{version}-windows-{arch}-setup.exe"),
        WindowsUpdateLayout::Portable(_) => format!("tty7-{version}-windows-{arch}.zip"),
    })
}

#[cfg(target_os = "windows")]
fn current_windows_update_layout() -> Option<WindowsUpdateLayout> {
    let executable = std::env::current_exe().ok()?;
    windows_update_layout_for(&executable)
}

#[cfg(target_os = "windows")]
fn windows_update_layout_for(executable: &Path) -> Option<WindowsUpdateLayout> {
    let directory = executable.parent()?;
    if directory.join(WINDOWS_INNO_INSTALL_MARKER).is_file() {
        return Some(WindowsUpdateLayout::Inno(directory.to_path_buf()));
    }
    let marker = std::fs::read(directory.join(WINDOWS_PORTABLE_MARKER)).ok()?;
    (marker == WINDOWS_PORTABLE_MARKER_CONTENT)
        .then(|| WindowsUpdateLayout::Portable(directory.to_path_buf()))
}

#[cfg(target_os = "windows")]
fn windows_directory_is_writable(directory: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(".tty7-update-write-test-")
        .tempfile_in(directory)
        .is_ok()
}

/// Rejects the Windows installation layouts that cannot be replaced by this
/// process, before anything is downloaded.
#[cfg(target_os = "windows")]
fn windows_layout_is_updatable(layout: &WindowsUpdateLayout) -> Result<(), UpdateInstallHint> {
    match layout {
        WindowsUpdateLayout::Inno(directory) => {
            if windows_inno_needs_elevation(directory) {
                return Err(UpdateInstallHint::WindowsAllUsersInstall);
            }
            Ok(())
        }
        WindowsUpdateLayout::Portable(directory) => {
            if !windows_directory_is_writable(directory) {
                return Err(UpdateInstallHint::UnsupportedWindows);
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "windows")]
fn windows_inno_needs_elevation(install_dir: &Path) -> bool {
    windows_inno_needs_elevation_for(
        windows_all_users_install_path().as_deref(),
        install_dir,
        windows_directory_is_writable(install_dir),
    )
}

/// Whether replacing this Inno installation would need administrator rights.
///
/// The updater runs the release Setup silently, as the signed-in user, from a
/// private staging directory. That is only correct for a per-user install.
/// Two independent signals, because either alone misreads a real machine:
///
///   * An all-users install records its state under `HKLM`. A silent Setup
///     launched without elevation resolves `{autopf}` to
///     `%LocalAppData%\Programs`, never sees that state, and installs a
///     *second* copy while the real installation goes untouched — or Inno
///     re-launches itself elevated and the user gets a bare UAC prompt for an
///     unsigned executable in `%TEMP%`, seconds after the GUI vanished.
///     Neither outcome is one tty7 should produce on its own initiative.
///   * A directory this process cannot write is one Setup cannot write
///     either, whatever the registry says. This also catches an installation
///     whose uninstall entry was pruned, relocated, or written by a different
///     user account.
///
/// Pure so the decision is unit-tested without touching the registry or
/// `C:\Program Files`.
#[cfg(target_os = "windows")]
fn windows_inno_needs_elevation_for(
    all_users_app_path: Option<&Path>,
    install_dir: &Path,
    writable: bool,
) -> bool {
    if all_users_app_path.is_some_and(|path| same_windows_directory(path, install_dir)) {
        return true;
    }
    !writable
}

/// Compares two Windows directory paths the way the filesystem does: without
/// regard to case, and without letting a trailing separator make
/// `C:\Program Files\tty7\` a different place from `C:\Program Files\tty7`.
/// Deliberately textual — `canonicalize` would hit the disk and answers
/// `\\?\`-prefixed, which is not what the registry stores.
#[cfg(target_os = "windows")]
fn same_windows_directory(left: &Path, right: &Path) -> bool {
    fn normalize(path: &Path) -> Option<String> {
        let text = path.to_str()?.trim_end_matches(['\\', '/']);
        (!text.is_empty()).then(|| text.to_lowercase())
    }
    match (normalize(left), normalize(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// The `{app}` directory of an all-users tty7 installation, read from the
/// machine hive. `AppId` is frozen in `windows-installer.iss` for exactly this
/// kind of lookup, and Inno stamps the resolved install directory into
/// `Inno Setup: App Path`. Absent for a per-user install, whose uninstall
/// entry lives under `HKCU` instead.
#[cfg(target_os = "windows")]
fn windows_all_users_install_path() -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };

    const UNINSTALL_KEY: &str = concat!(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\",
        r"{9A3F6C1E-4B7D-4E2A-8C5F-D01B92E64A37}_is1"
    );
    const APP_PATH_VALUE: &str = "Inno Setup: App Path";

    struct RegistryKey(HKEY);

    impl Drop for RegistryKey {
        fn drop(&mut self) {
            // SAFETY: only constructed from a successful `RegOpenKeyExW`, and
            // owns exactly one handle.
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let path = wide(UNINSTALL_KEY);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: `path` is NUL-terminated and live for the call; `key` is a valid
    // out-parameter, wrapped only when the call reports success. The tty7
    // installer is x64-only, so the native 64-bit view is the only one its
    // uninstall entry can appear in.
    let code = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut key) };
    if code != ERROR_SUCCESS {
        return None;
    }
    let key = RegistryKey(key);

    let name = wide(APP_PATH_VALUE);
    let mut kind = 0u32;
    let mut bytes = 0u32;
    // SAFETY: the key is live, the value name is NUL-terminated, and the
    // type/size out-parameters are valid; a null data pointer asks for the
    // size only.
    let code = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if code != ERROR_SUCCESS || kind != REG_SZ || bytes == 0 || !bytes.is_multiple_of(2) {
        return None;
    }

    let mut value = vec![0u16; bytes as usize / 2];
    // SAFETY: `value` is sized from the query above and stays live; Win32 is
    // told its capacity in bytes through `bytes`.
    let code = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            value.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if code != ERROR_SUCCESS {
        return None;
    }
    value.truncate(bytes as usize / 2);
    while value.last() == Some(&0) {
        value.pop();
    }
    (!value.is_empty()).then(|| PathBuf::from(std::ffi::OsString::from_wide(&value)))
}

#[cfg(target_os = "macos")]
fn can_stage_replacement_in(dir: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(".tty7-update-write-test-")
        .tempfile_in(dir)
        .is_ok()
}

fn run_updater(updater: &Path, args: impl IntoIterator<Item = PathBuf>) -> Result<()> {
    let mut command = Command::new(updater);
    command.args(args);
    let output = tty7_core::core::proc::hide_console(&mut command)
        .output()
        .context("running tty7-updater verification")?;
    if !output.status.success() {
        anyhow::bail!(
            "tty7-updater verification failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(())
}

/// `(major, minor, patch, is_release)`. Ordering the release flag last, with
/// `false < true`, is what lets a prerelease be superseded by the stable
/// release that carries the same core version: a Nightly stamped
/// `26.7.1-nightly.20260716` is offered `v26.7.1` and graduates out of the
/// prerelease. Two prereleases sharing a core compare equal, so nothing here
/// can walk a user from one prerelease to another — only `/releases/latest`
/// feeds this comparison, and that endpoint never returns one.
fn parse_version(s: &str) -> Option<(u64, u64, u64, bool)> {
    let trimmed = s.trim();
    let core = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let is_release = !core.split('+').next().unwrap_or(core).contains('-');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch, is_release))
}

fn is_update_available(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github_asset(name: &str) -> GitHubAsset {
        GitHubAsset {
            name: name.to_string(),
            browser_download_url: format!("https://example.test/{name}"),
        }
    }

    #[test]
    fn release_asset_requires_the_platform_package_and_checksums() {
        let name = "tty7-27.1.0-macos-arm64.zip";
        let assets = [github_asset(name), github_asset("checksums.txt")];
        let selected = select_release_asset_for(Ok(name.to_string()), &assets);
        assert_eq!(
            selected.asset,
            Some(ReleaseAsset {
                name: name.to_string(),
                url: format!("https://example.test/{name}"),
                checksums_url: "https://example.test/checksums.txt".to_string(),
            })
        );
        assert_eq!(selected.reason, None);
    }

    #[test]
    fn release_without_checksums_is_never_installable() {
        let name = "tty7-27.1.0-macos-arm64.zip";
        let selected = select_release_asset_for(Ok(name.to_string()), &[github_asset(name)]);
        assert!(selected.asset.is_none());
        assert_eq!(selected.reason, Some(UpdateInstallHint::MissingChecksums));
    }

    #[test]
    fn release_without_the_exact_platform_package_is_never_guessed() {
        let selected = select_release_asset_for(
            Ok("tty7-27.1.0-macos-arm64.zip".to_string()),
            &[
                github_asset("tty7-27.1.0-macos-x86_64.zip"),
                github_asset("checksums.txt"),
            ],
        );
        assert!(selected.asset.is_none());
        assert_eq!(
            selected.reason,
            Some(UpdateInstallHint::MissingPackage(
                "tty7-27.1.0-macos-arm64.zip".to_string()
            ))
        );
    }

    #[test]
    fn parses_versions_with_and_without_prefix() {
        assert_eq!(parse_version("v0.3.1"), Some((0, 3, 1, true)));
        assert_eq!(parse_version("0.3.1"), Some((0, 3, 1, true)));
        assert_eq!(parse_version(" 1.2.0 "), Some((1, 2, 0, true)));
        assert_eq!(parse_version("v2"), Some((2, 0, 0, true)));
        assert_eq!(parse_version("v2.5"), Some((2, 5, 0, true)));
        assert_eq!(parse_version("v0.4.0-rc.1"), Some((0, 4, 0, false)));
        assert_eq!(
            parse_version("26.7.1-nightly.20260716"),
            Some((26, 7, 1, false))
        );
        assert_eq!(parse_version("0.4.0+ci.7"), Some((0, 4, 0, true)));
        assert_eq!(parse_version("nightly"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("v0.3.1.1"), None);
        assert_eq!(parse_version("0.3.1.0"), None);
        assert_eq!(parse_version("vv0.3.1"), None);
    }

    #[test]
    fn detects_newer_versions() {
        assert!(is_update_available("v0.3.1", "0.3.0"));
        assert!(is_update_available("v1.0.0", "0.9.9"));
        assert!(is_update_available("0.4.0", "0.3.99"));
        assert!(is_update_available("v26.7.0", "0.17.0"));
    }

    #[test]
    fn ignores_same_or_older_versions() {
        assert!(!is_update_available("v0.3.0", "0.3.0"));
        assert!(!is_update_available("v0.2.9", "0.3.0"));
        assert!(!is_update_available("0.3.0", "0.3.1"));
    }

    #[test]
    fn nightly_binaries_prompt_when_their_stable_ships() {
        assert!(is_update_available("v26.7.1", "26.7.1-nightly.20260716"));
        assert!(!is_update_available("v26.7.0", "26.7.1-nightly.20260716"));
        assert!(!is_update_available("v26.7.1-rc.1", "26.7.1"));
        // Nightly is a build channel, not an update channel: one Nightly never
        // supersedes another. `/releases/latest` cannot return a prerelease, so
        // this pair is unreachable in practice — asserted so a future change to
        // the endpoint cannot quietly turn Nightly into an update source.
        assert!(!is_update_available(
            "26.7.1-nightly.20260717",
            "26.7.1-nightly.20260716"
        ));
    }

    #[test]
    fn unparseable_tag_never_prompts() {
        assert!(!is_update_available("garbage", "0.3.0"));
        assert!(!is_update_available("v0.3.1", "garbage"));
        assert!(!is_update_available("v0.4.0.1", "0.3.0"));
        assert!(!is_update_available("vv0.4.0", "0.3.0"));
    }

    #[test]
    fn update_state_round_trips_and_defaults() {
        crate::core::config::pin_test_config_dir();
        let path = UpdateState::path().expect("config dir pinned");

        let _ = std::fs::remove_file(&path);
        assert_eq!(UpdateState::load().last_prompted, None);

        UpdateState {
            last_prompted: Some("0.4.0".into()),
        }
        .save();
        assert_eq!(UpdateState::load().last_prompted.as_deref(), Some("0.4.0"));

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_markers_distinguish_inno_portable_and_unknown_layouts() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("tty7-app.exe");
        std::fs::write(&executable, b"test app").unwrap();
        assert_eq!(windows_update_layout_for(&executable), None);

        std::fs::write(root.path().join(WINDOWS_PORTABLE_MARKER), b"portable-v1").unwrap();
        assert_eq!(
            windows_update_layout_for(&executable),
            Some(WindowsUpdateLayout::Portable(root.path().to_path_buf()))
        );

        std::fs::write(root.path().join(WINDOWS_INNO_INSTALL_MARKER), b"inno-v1").unwrap();
        assert_eq!(
            windows_update_layout_for(&executable),
            Some(WindowsUpdateLayout::Inno(root.path().to_path_buf()))
        );

        std::fs::remove_file(root.path().join(WINDOWS_INNO_INSTALL_MARKER)).unwrap();
        std::fs::write(root.path().join(WINDOWS_PORTABLE_MARKER), b"invalid").unwrap();
        assert_eq!(windows_update_layout_for(&executable), None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_layout_selects_the_matching_release_package() {
        let directory = PathBuf::from(r"C:\tty7");
        assert_eq!(
            windows_package_for_layout("26.8.2", &WindowsUpdateLayout::Inno(directory.clone()))
                .as_deref(),
            Some("tty7-26.8.2-windows-x86_64-setup.exe")
        );
        assert_eq!(
            windows_package_for_layout("26.8.2", &WindowsUpdateLayout::Portable(directory))
                .as_deref(),
            Some("tty7-26.8.2-windows-x86_64.zip")
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn an_all_users_inno_install_is_never_updated_in_place() {
        let all_users = PathBuf::from(r"C:\Program Files\tty7");
        let per_user = PathBuf::from(r"C:\Users\someone\AppData\Local\Programs\tty7");

        // The machine-hive entry names this directory: elevation would be
        // required, so tty7 declines however writable the directory looks.
        assert!(windows_inno_needs_elevation_for(
            Some(&all_users),
            &all_users,
            true
        ));
        // Inno stores the path with a trailing separator in `InstallLocation`
        // and without one in `Inno Setup: App Path`; both name one place.
        assert!(windows_inno_needs_elevation_for(
            Some(Path::new(r"C:\Program Files\tty7\")),
            &all_users,
            true
        ));
        assert!(windows_inno_needs_elevation_for(
            Some(Path::new(r"c:\program files\TTY7")),
            &all_users,
            true
        ));

        // A per-user install on a machine that also carries an all-users one
        // updates itself: the machine entry names a different directory.
        assert!(!windows_inno_needs_elevation_for(
            Some(&all_users),
            &per_user,
            true
        ));
        assert!(!windows_inno_needs_elevation_for(None, &per_user, true));

        // No machine entry, but the directory refuses writes — a relocated or
        // pruned installation Setup could not replace either.
        assert!(windows_inno_needs_elevation_for(None, &per_user, false));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn an_all_users_inno_install_reports_the_elevation_hint() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("tty7-app.exe");
        std::fs::write(&executable, b"test app").unwrap();
        std::fs::write(root.path().join(WINDOWS_INNO_INSTALL_MARKER), b"inno-v1").unwrap();
        let layout = windows_update_layout_for(&executable).unwrap();

        // A writable temp directory is never the all-users installation, so
        // this layout is offered the normal in-place update.
        assert_eq!(windows_layout_is_updatable(&layout), Ok(()));

        assert_eq!(
            select_release_asset_for(Err(UpdateInstallHint::WindowsAllUsersInstall), &[]).reason,
            Some(UpdateInstallHint::WindowsAllUsersInstall)
        );
        let hint = UpdateInstallHint::WindowsAllUsersInstall.english();
        assert!(hint.contains("all users"), "{hint}");
        assert!(hint.contains("release page"), "{hint}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn an_unwritable_portable_directory_is_not_offered_an_update() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().to_path_buf();
        assert!(windows_directory_is_writable(&directory));
        assert_eq!(
            windows_layout_is_updatable(&WindowsUpdateLayout::Portable(directory)),
            Ok(())
        );

        let missing = root.path().join("gone");
        assert!(!windows_directory_is_writable(&missing));
        assert_eq!(
            windows_layout_is_updatable(&WindowsUpdateLayout::Portable(missing)),
            Err(UpdateInstallHint::UnsupportedWindows)
        );
    }

    /// Reads the real machine hive. Vacuous on a machine with no all-users
    /// installation; on one that has it, the value Inno actually wrote must be
    /// an absolute path and must make the decision function refuse an in-place
    /// update of that directory.
    #[cfg(target_os = "windows")]
    #[test]
    fn the_all_users_install_path_lookup_survives_this_machine() {
        let Some(path) = windows_all_users_install_path() else {
            return;
        };
        assert!(path.is_absolute(), "{}", path.display());
        assert!(
            windows_inno_needs_elevation_for(Some(&path), &path, true),
            "the installed all-users path {} was not recognised",
            path.display()
        );
    }
}

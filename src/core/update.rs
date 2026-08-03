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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub version: String,
    pub installable: bool,
    pub install_hint: Option<String>,
    asset: Option<ReleaseAsset>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Downloading,
    Installing,
    Failed(String),
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
                    let message = format!("Could not check for updates: {e:#}");
                    cx.update(|cx| {
                        set_status(
                            UpdateStatus {
                                available: previous_available,
                                phase: UpdatePhase::Failed(message),
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
    let detail = if update.installable {
        let note = update
            .install_hint
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
            update
                .install_hint
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
                let message = format!("Update failed: {error:#}");
                log::error!("{message}");
                cx.update(|cx| {
                    set_status(
                        UpdateStatus {
                            available: Some(update),
                            phase: UpdatePhase::Failed(message),
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
                let message = format!("Could not start the installer: {error:#}");
                log::error!("{message}");
                cx.update(|cx| {
                    set_status(
                        UpdateStatus {
                            available: Some(update),
                            phase: UpdatePhase::Failed(message),
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
    reason: Option<String>,
}

fn select_release_asset(version: &str, assets: &[GitHubAsset]) -> AssetSelection {
    select_release_asset_for(package_for_current_install(version), assets)
}

fn select_release_asset_for(package: Option<String>, assets: &[GitHubAsset]) -> AssetSelection {
    let Some(name) = package else {
        return AssetSelection {
            asset: None,
            reason: Some(unsupported_install_reason()),
        };
    };
    let Some(asset) = assets.iter().find(|asset| asset.name == name) else {
        return AssetSelection {
            asset: None,
            reason: Some(format!(
                "The release has no {name} package for this installation. Open the release page \
                 to choose another package."
            )),
        };
    };
    let Some(checksums) = assets.iter().find(|asset| asset.name == "checksums.txt") else {
        return AssetSelection {
            asset: None,
            reason: Some(
                "The release has no checksums.txt, so tty7 refuses to install it automatically."
                    .to_string(),
            ),
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

fn package_for_current_install(version: &str) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let app = current_macos_app_bundle()?;
        if !is_macos_update_writable(&app) || bundled_updater().is_none() {
            return None;
        }
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else {
            return None;
        };
        return Some(format!("tty7-{version}-macos-{arch}.zip"));
    }
    #[allow(unreachable_code)]
    None
}

fn unsupported_install_reason() -> String {
    #[cfg(target_os = "macos")]
    {
        return "This copy is not running from a writable tty7.app bundle, so replacing it would be \
                unsafe. Move tty7 to Applications or another writable folder, or open the release \
                page to install the update."
            .to_string();
    }
    #[cfg(target_os = "linux")]
    {
        return "The first in-app updater supports packaged macOS app bundles. Use the release page \
                or your package manager to update this Linux installation."
            .to_string();
    }
    #[cfg(target_os = "windows")]
    {
        return "The first in-app updater supports packaged macOS app bundles. Open the release page \
                to update this Windows installation."
            .to_string();
    }
    #[allow(unreachable_code)]
    "Automatic installation is not available on this platform. Open the release page.".to_string()
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
    prepare_macos_update(version, &asset.name, &archive, &checksums)
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
        command
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

fn update_staging_dir(parent: &Path) -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(".tty7-update-")
        .tempdir_in(parent)
        .context("creating update staging directory")
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
            archive,
            checksums,
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
            dir.clone(),
            PathBuf::from(version),
            log,
        ],
        config_dir: crate::core::config::config_dir_path(),
        stage: dir,
    })
}

#[cfg(not(target_os = "macos"))]
fn prepare_macos_update(
    _version: &str,
    _asset_name: &str,
    _archive: &[u8],
    _checksums: &[u8],
) -> Result<PreparedUpdate> {
    anyhow::bail!("the first in-app updater only supports macOS")
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

#[cfg(not(target_os = "macos"))]
fn bundled_updater() -> Option<PathBuf> {
    None
}

#[cfg(not(target_os = "macos"))]
fn current_macos_app_bundle() -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
fn can_stage_replacement_in(dir: &Path) -> bool {
    tempfile::Builder::new()
        .prefix(".tty7-update-write-test-")
        .tempfile_in(dir)
        .is_ok()
}

fn run_updater(updater: &Path, args: impl IntoIterator<Item = PathBuf>) -> Result<()> {
    let output = Command::new(updater)
        .args(args)
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
        let selected = select_release_asset_for(Some(name.to_string()), &assets);
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
        let selected = select_release_asset_for(Some(name.to_string()), &[github_asset(name)]);
        assert!(selected.asset.is_none());
        assert!(
            selected
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("checksums.txt"))
        );
    }

    #[test]
    fn release_without_the_exact_platform_package_is_never_guessed() {
        let selected = select_release_asset_for(
            Some("tty7-27.1.0-macos-arm64.zip".to_string()),
            &[
                github_asset("tty7-27.1.0-macos-x86_64.zip"),
                github_asset("checksums.txt"),
            ],
        );
        assert!(selected.asset.is_none());
        assert!(
            selected
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("macos-arm64"))
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
}

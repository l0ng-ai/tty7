use anyhow::{Context as _, Result};
use gpui::http_client::{AsyncBody, HttpClient as _, HttpRequestExt as _, RedirectPolicy};
use gpui::{AnyWindowHandle, App, AsyncApp, Global, PromptLevel, Window, http_client};
use reqwest_client::ReqwestClient;
use smol::future::FutureExt as _;
use smol::io::AsyncReadExt as _;
use std::time::Duration;

use crate::core::config::Config;

const REPO: &str = "l0ng-ai/tty7";

pub const RELEASES_URL: &str = "https://github.com/l0ng-ai/tty7/releases/latest";

const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub version: String,
}

#[derive(Clone, Debug, Default)]
pub struct UpdateStatus {
    pub available: Option<AvailableUpdate>,
}

impl Global for UpdateStatus {}

pub fn spawn_check(cx: &mut App) {
    if !cx.global::<Config>().check_for_updates {
        return;
    }
    spawn_check_forced(cx);
}

pub fn spawn_check_forced(cx: &mut App) {
    cx.spawn(async move |cx| {
        let current = env!("CARGO_PKG_VERSION");
        let latest = match fetch_latest_version()
            .or(async {
                cx.background_executor().timer(CHECK_TIMEOUT).await;
                Err(anyhow::anyhow!("timed out after {CHECK_TIMEOUT:?}"))
            })
            .await
        {
            Ok(v) => v,
            Err(e) => {
                log::debug!("update check skipped: {e:#}");
                return;
            }
        };

        if !is_update_available(&latest, current) {
            log::debug!("update check: up to date (latest {latest}, running {current})");
            return;
        }

        let version = latest.trim_start_matches('v').to_string();
        log::info!("update available: {version} (running {current})");

        cx.update(|cx| {
            cx.set_global(UpdateStatus {
                available: Some(AvailableUpdate {
                    version: version.clone(),
                }),
            });
            cx.refresh_windows();
        });

        if UpdateState::load().last_prompted.as_deref() == Some(version.as_str()) {
            return;
        }

        let Some(window) = wait_for_window(cx).await else {
            return;
        };
        let shown = cx.update(|cx| {
            window
                .update(cx, |_root, window, cx| prompt_update(&version, window, cx))
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

fn prompt_update(version: &str, window: &mut Window, cx: &mut App) {
    let detail = format!(
        "tty7 {version} is available — you're on {}. Open the download page to get it.",
        env!("CARGO_PKG_VERSION")
    );
    let answer = window.prompt(
        PromptLevel::Info,
        "Update available",
        Some(&detail),
        &["Later", "Download"],
        cx,
    );
    cx.spawn(async move |_cx| {
        if let Ok(1) = answer.await {
            open_releases_page();
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

#[derive(serde::Deserialize)]
struct LatestRelease {
    tag_name: String,
}

async fn fetch_latest_version() -> Result<String> {
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
    Ok(release.tag_name)
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

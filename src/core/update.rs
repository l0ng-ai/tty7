//! Notify-only update check.
//!
//! On GUI startup (unless `config.check_for_updates` is off) we make one GET to
//! the GitHub releases API, compare the latest published version against the
//! running binary, and — if it's newer — stash an [`UpdateStatus`] global that
//! Settings → About reads to show a "download" prompt linking to the Releases
//! page. That's the whole feature: we never download, replace, or restart
//! anything. The user updates by hand (drag the new `.app`, unzip, …), exactly
//! as the README's Install section describes.
//!
//! Everything here fails soft: no network, a rate-limit, a private/renamed repo,
//! an unparseable tag — all collapse to "no prompt", logged at `debug` and never
//! surfaced. A terminal must open the same whether or not GitHub is reachable.

use anyhow::{Context as _, Result};
use gpui::http_client::{AsyncBody, HttpClient as _, HttpRequestExt as _, RedirectPolicy};
use gpui::{AnyWindowHandle, App, AsyncApp, Global, PromptLevel, Window, http_client};
use reqwest_client::ReqwestClient;
use smol::future::FutureExt as _;
use smol::io::AsyncReadExt as _;
use std::time::Duration;

use crate::core::config::Config;

/// `owner/repo` the release check queries — matches the repository the binary is
/// published from (see `Cargo.toml`'s `repository`).
const REPO: &str = "l0ng-ai/tty7";

/// Where the "Download" prompt points. GitHub's `/releases/latest` alias always
/// resolves to the newest published (non-prerelease) build, so it never goes
/// stale as versions roll — no need to embed a specific tag.
pub const RELEASES_URL: &str = "https://github.com/l0ng-ai/tty7/releases/latest";

/// Overall wall-clock budget for the check. `ReqwestClient` only sets a *connect*
/// timeout, so without this a connected-but-stalled response could sit pending
/// for the whole session; racing a timer keeps the "fail soft" behavior
/// deterministic instead of open-ended.
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// A newer release than the one currently running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableUpdate {
    /// The newer version, normalized without a leading `v` (e.g. `"0.3.1"`), for
    /// display in the About panel.
    pub version: String,
}

/// The result of the startup update check, stored as a GPUI global so the
/// Settings view can read it. Absent until the check completes; `available` is
/// `None` when we're already current (or the check failed / was skipped).
#[derive(Clone, Debug, Default)]
pub struct UpdateStatus {
    pub available: Option<AvailableUpdate>,
}

impl Global for UpdateStatus {}

/// Kick off the background update check. Returns immediately; the network work
/// runs on a detached task and, if a newer version exists:
///   1. writes the [`UpdateStatus`] global (the passive Settings → About prompt,
///      shown on every launch while outdated), and
///   2. pops a one-time modal dialog for that version — but only the first
///      launch it's seen; the version is remembered in `update.json` so we never
///      nag twice for the same release.
///
/// Honors `config.check_for_updates`: when off, we make no network call at all.
pub fn spawn_check(cx: &mut App) {
    if !cx.global::<Config>().check_for_updates {
        return;
    }
    spawn_check_forced(cx);
}

/// The same check, ignoring the `check_for_updates` toggle — for explicit
/// user-initiated checks (the tray's "Check for Updates…"), where "I asked"
/// overrides "don't ask on my behalf at startup".
pub fn spawn_check_forced(cx: &mut App) {
    cx.spawn(async move |cx| {
        let current = env!("CARGO_PKG_VERSION");
        // Race the fetch against a timer so a stalled connection can't leave the
        // task pending forever; whichever finishes first wins.
        let latest = match fetch_latest_version()
            .or(async {
                cx.background_executor().timer(CHECK_TIMEOUT).await;
                Err(anyhow::anyhow!("timed out after {CHECK_TIMEOUT:?}"))
            })
            .await
        {
            Ok(v) => v,
            Err(e) => {
                // `{e:#}` includes the anyhow context chain; kept at debug so a
                // routine offline start doesn't spam the log.
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

        // Record it for the passive Settings → About prompt and repaint so an
        // already-open About picks it up now rather than on the next interaction.
        cx.update(|cx| {
            cx.set_global(UpdateStatus {
                available: Some(AvailableUpdate {
                    version: version.clone(),
                }),
            });
            cx.refresh_windows();
        });

        // Active modal: pop exactly once per version. If a previous launch
        // already showed it for this version, stop here — About still carries
        // the passive prompt.
        if UpdateState::load().last_prompted.as_deref() == Some(version.as_str()) {
            return;
        }

        // The check can outrace the window-open task at startup; wait briefly
        // for a window to host the modal before giving up.
        let Some(window) = wait_for_window(cx).await else {
            return;
        };
        let shown = cx.update(|cx| {
            window
                .update(cx, |_root, window, cx| prompt_update(&version, window, cx))
                .is_ok()
        });

        // Persist only after the modal actually went up, so a version we never
        // managed to show still gets its one prompt on a later launch.
        if shown {
            UpdateState {
                last_prompted: Some(version),
            }
            .save();
        }
    })
    .detach();
}

/// Poll (briefly) for the app's main window. Returns `None` if none appears
/// within the window — treated as "no host for the modal", so we simply skip it.
async fn wait_for_window(cx: &mut AsyncApp) -> Option<AnyWindowHandle> {
    // ~5s of 100ms ticks. The network round-trip almost always finishes after
    // the window is already up, so this usually returns on the first poll.
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

/// Show the one-time "update available" modal, and open the Releases page if the
/// user picks Download. Mirrors the window-close confirmation's prompt style.
fn prompt_update(version: &str, window: &mut Window, cx: &mut App) {
    let detail = format!(
        "tty7 {version} is available — you're on {}. Open the download page to get it.",
        env!("CARGO_PKG_VERSION")
    );
    // Index 1 == "Download"; index 0 (Later) and a dismissed prompt do nothing.
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

/// Open the GitHub Releases page with the OS default handler. Shared by the
/// modal's Download button and the Settings → About Download button.
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

/// Tiny persisted state for the update checker, stored at `update.json` in the
/// config dir (alongside `config.json` / `session.json`). Currently just the
/// last version we popped the modal for, so we never nag twice for one release.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct UpdateState {
    #[serde(default)]
    last_prompted: Option<String>,
}

impl UpdateState {
    fn path() -> Option<std::path::PathBuf> {
        crate::core::config::config_path("update.json")
    }

    /// Load persisted state; a missing / unreadable / malformed file all yield
    /// the default (never prompted), so at worst we prompt once more.
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

    /// Persist state; IO / serialization errors are logged and swallowed.
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

/// The `tag_name` field of GitHub's release payload — the only piece we read.
#[derive(serde::Deserialize)]
struct LatestRelease {
    tag_name: String,
}

/// GET the repo's latest release and return its raw tag (e.g. `"v0.3.1"`).
async fn fetch_latest_version() -> Result<String> {
    // GitHub rejects requests without a User-Agent; identify ourselves. The
    // reqwest+rustls stack this rides on is already compiled into the app via
    // `gpui-component-assets`, so constructing a client here is cheap.
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

/// Parse a version string into a `(major, minor, patch, is_release)` tuple,
/// tolerating a leading `v`. Missing minor/patch components read as `0`.
/// Returns `None` if the numeric core doesn't parse — the caller treats that
/// as "don't prompt".
///
/// The trailing `is_release` flag implements semver's pre-release ordering
/// through plain tuple comparison: a `-nightly.20260716` (or `-rc.1`) suffix
/// makes it `false`, which sorts *below* the same core with `true` — so a
/// nightly binary counts as older than the stable release it previews, and the
/// prompt fires when that stable ships. Finer ordering *between* pre-releases
/// isn't needed: the check only ever compares against `/releases/latest`,
/// which never returns a pre-release. Build metadata (`+ci`) is ignored, as
/// semver says it should be.
fn parse_version(s: &str) -> Option<(u64, u64, u64, bool)> {
    let trimmed = s.trim();
    // Strip a single optional `v` prefix. `strip_prefix` (not `trim_start_matches`)
    // so a doubled `vv0.3.1` fails to parse instead of silently losing the extra v.
    let core = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let is_release = !core.split('+').next().unwrap_or(core).contains('-');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    // Reject extra components (`0.3.1.1`) rather than truncating them: an
    // unrecognizable tag must never surface a bogus "update available".
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch, is_release))
}

/// Whether `latest` names a strictly newer version than `current`. If either
/// side fails to parse we return `false`: an unrecognizable tag should never
/// nag the user to "update" to something we can't even order.
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
        // Missing components default to zero.
        assert_eq!(parse_version("v2"), Some((2, 0, 0, true)));
        assert_eq!(parse_version("v2.5"), Some((2, 5, 0, true)));
        // A pre-release suffix keeps the core but sorts below the release…
        assert_eq!(parse_version("v0.4.0-rc.1"), Some((0, 4, 0, false)));
        assert_eq!(
            parse_version("26.7.1-nightly.20260716"),
            Some((26, 7, 1, false))
        );
        // …while build metadata alone is still a release.
        assert_eq!(parse_version("0.4.0+ci.7"), Some((0, 4, 0, true)));
        // Garbage yields None.
        assert_eq!(parse_version("nightly"), None);
        assert_eq!(parse_version(""), None);
        // Malformed cores are rejected, not truncated into a bogus version:
        // extra components or a doubled prefix must not parse.
        assert_eq!(parse_version("v0.3.1.1"), None);
        assert_eq!(parse_version("0.3.1.0"), None);
        assert_eq!(parse_version("vv0.3.1"), None);
    }

    #[test]
    fn detects_newer_versions() {
        assert!(is_update_available("v0.3.1", "0.3.0"));
        assert!(is_update_available("v1.0.0", "0.9.9"));
        assert!(is_update_available("0.4.0", "0.3.99"));
        // CalVer jump over the old 0.x tags still orders correctly.
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
        // A nightly previews the next stable: same core, sorts below it.
        assert!(is_update_available("v26.7.1", "26.7.1-nightly.20260716"));
        // …but the stable it was built *after* is not an update.
        assert!(!is_update_available("v26.7.0", "26.7.1-nightly.20260716"));
        // A stable binary is never downgraded to a pre-release of itself.
        assert!(!is_update_available("v26.7.1-rc.1", "26.7.1"));
        // Nightly-to-nightly is deliberately not an update: pre-releases with
        // the same core compare equal, and the check only ever sees
        // /releases/latest, which is never a pre-release anyway.
        assert!(!is_update_available(
            "26.7.1-nightly.20260717",
            "26.7.1-nightly.20260716"
        ));
    }

    #[test]
    fn unparseable_tag_never_prompts() {
        assert!(!is_update_available("garbage", "0.3.0"));
        assert!(!is_update_available("v0.3.1", "garbage"));
        // A malformed newer-looking tag must not surface a bogus update.
        assert!(!is_update_available("v0.4.0.1", "0.3.0"));
        assert!(!is_update_available("vv0.4.0", "0.3.0"));
    }

    #[test]
    fn update_state_round_trips_and_defaults() {
        // Pin a throwaway config dir (first-call-wins; same scheme the session
        // tests use, so the whole test binary shares one temp dir).
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir);
        let path = UpdateState::path().expect("config dir pinned");

        // Missing file → default (never prompted), so we'd prompt.
        let _ = std::fs::remove_file(&path);
        assert_eq!(UpdateState::load().last_prompted, None);

        // A recorded version round-trips, so a second launch skips the modal.
        UpdateState {
            last_prompted: Some("0.4.0".into()),
        }
        .save();
        assert_eq!(UpdateState::load().last_prompted.as_deref(), Some("0.4.0"));

        // Don't leak state into other runs sharing the pinned dir.
        let _ = std::fs::remove_file(&path);
    }
}

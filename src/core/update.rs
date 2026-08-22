use anyhow::{Context as _, Result};
use gpui::http_client::{AsyncBody, HttpClient as _, HttpRequestExt as _, RedirectPolicy};
use gpui::{AnyWindowHandle, App, AsyncApp, Global, PromptLevel, Window, http_client};
use reqwest_client::ReqwestClient;
use smol::future::FutureExt as _;
use smol::io::AsyncReadExt as _;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tty7_core::daemon::install::AssetFetcher as _;

use crate::core::config::{Config, UpdateChannel};
use crate::ui::i18n::{L10nKey, t, t_fmt};

const REPO: &str = "l0ng-ai/tty7";

/// The rolling prerelease the Nightly channel follows. Force-moved to a new
/// commit every night, which is exactly why it cannot double as a version.
const NIGHTLY_TAG: &str = "nightly";

/// Published beside the nightly packages so the version is stated rather than
/// inferred. See `resolve_version`.
const NIGHTLY_MANIFEST: &str = "nightly.json";

pub const RELEASES_URL: &str = "https://github.com/l0ng-ai/tty7/releases/latest";

/// The nightly release's own page. Unlike Stable's, this URL is stable across
/// nights — the tag stays put even as the commit under it moves. Spelled out
/// rather than built from `NIGHTLY_TAG`, which `concat!` cannot take; the tail
/// is asserted against it in `each_channel_reads_its_own_feed` instead.
pub const NIGHTLY_RELEASE_URL: &str = "https://github.com/l0ng-ai/tty7/releases/tag/nightly";

const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// A terminal workbench is left open for weeks, so a launch-only check reaches
/// only the people who restart anyway — the ones who would have found the
/// update themselves.
const RECHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// How long "Later" silences one version for. It has to be a real delay rather
/// than the old behaviour (prompted once, then silent forever), or declining an
/// update once removes it from the user's world permanently.
const REMIND_LATER: Duration = Duration::from_secs(3 * 24 * 60 * 60);

/// Staging directories older than this belong to a run that died before its
/// updater could clean up — a download the user cancelled by quitting, or a
/// crash. On macOS they sit in /Applications, so nobody finds them by accident.
const STAGE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Set by `cancel_download`, read by the transfer's progress callback. One
/// download runs at a time (`spawn_download` refuses to start a second), so a
/// single flag is the whole mechanism.
static DOWNLOAD_CANCELLED: AtomicBool = AtomicBool::new(false);

/// Bumped by `switch_channel`. A download carries the value it started under,
/// so a package prepared for the feed the user just left is thrown away instead
/// of being staged. `DOWNLOAD_CANCELLED` handles the common case — a transfer
/// still reading bytes — but stops being observed once the download is through
/// and the work moves on to hashing and unpacking, and that tail is exactly
/// long enough for a switch to land inside it.
static CHANNEL_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Whether the user has asked to install as soon as the package is ready. Set
/// by pressing the install button while a background download is still going,
/// which is the common case now that checking starts one on its own.
static INSTALL_WHEN_READY: AtomicBool = AtomicBool::new(false);

/// Whether the staged package should be applied by the next launch without
/// asking again.
static APPLY_ON_LAUNCH: AtomicBool = AtomicBool::new(false);

/// Byte counters the download thread publishes and the UI samples. Cheaper and
/// less fussy than a channel for something the user reads a few times a second.
static DOWNLOAD_RECEIVED: AtomicU64 = AtomicU64::new(0);
/// Zero when the server sent no usable `content-length`.
static DOWNLOAD_TOTAL: AtomicU64 = AtomicU64::new(0);
/// Set once the bytes are in and the hashing, unpacking and signature checks
/// start. That work has no progress to report, and a percentage frozen at 100
/// is indistinguishable from a hang.
static DOWNLOAD_VERIFYING: AtomicBool = AtomicBool::new(false);

const PROGRESS_TICK: Duration = Duration::from_millis(120);

#[cfg(target_os = "windows")]
const WINDOWS_INNO_INSTALL_MARKER: &str = ".tty7-inno-install";
#[cfg(target_os = "windows")]
const WINDOWS_PORTABLE_MARKER: &str = ".tty7-portable";
#[cfg(target_os = "windows")]
const WINDOWS_PORTABLE_MARKER_CONTENT: &[u8] = b"portable-v1";
/// What the updater names the directory it moves the old portable files into.
#[cfg(target_os = "windows")]
const WINDOWS_PORTABLE_BACKUP_PREFIX: &str = ".tty7-update-backup-";
/// Present inside that backup from before the first installed file moves
/// until the replacement is complete — see `PORTABLE_BACKUP_INCOMPLETE` in
/// src/bin/tty7-updater.rs, which this duplicates like the markers above.
#[cfg(target_os = "windows")]
const WINDOWS_PORTABLE_BACKUP_INCOMPLETE: &str = ".tty7-replace-incomplete";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub version: String,
    pub installable: bool,
    pub install_hint: Option<UpdateInstallHint>,
    /// Windows, all-users Inno layout: installable, but the install raises
    /// one UAC prompt (#504). The dialog says so up front — a prompt the
    /// user was told about is consent; one that appears uninvited is not.
    #[cfg(target_os = "windows")]
    pub needs_elevation: bool,
    asset: Option<ReleaseAsset>,
}

impl AvailableUpdate {
    /// Whether installing this package raises a UAC prompt. A method rather
    /// than the field so dialog code stays cross-platform: no other layout
    /// ever elevates.
    #[cfg(target_os = "windows")]
    fn needs_elevation(&self) -> bool {
        self.needs_elevation
    }

    #[cfg(not(target_os = "windows"))]
    fn needs_elevation(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateInstallHint {
    #[cfg(target_os = "macos")]
    UnsupportedMacos,
    #[cfg(target_os = "linux")]
    UnsupportedLinux,
    /// The Linux shapes that still update by hand — a tarball install, or an
    /// AppImage that cannot replace itself (read-only directory, no bundled
    /// helper). "Use the release page" would leave the user to work out which
    /// of five files is theirs; this names it.
    #[cfg(target_os = "linux")]
    LinuxManualPackage(String),
    #[cfg(target_os = "windows")]
    UnsupportedWindows,
    #[cfg(target_os = "windows")]
    WindowsAllUsersInstall,
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    UnsupportedPlatform,
    MissingPackage(String),
    MissingChecksums,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Downloading {
        received: u64,
        total: Option<u64>,
    },
    /// Hashing the package and running the updater's pre-flight checks. Split
    /// out from `Downloading` because it is the part with no progress to show,
    /// and a percentage frozen at 100 reads as a hang.
    Verifying,
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
    /// A package that is downloaded, verified and waiting. Mirrors what is on
    /// disk in `update.json` so the UI can offer "install now" without knowing
    /// where the staging directory is.
    pub ready: Option<PendingUpdate>,
    /// The last failure, carried across restarts. A failure that evaporates
    /// when the app closes is one the user can neither act on nor report.
    pub failure: Option<FailureRecord>,
}

impl Global for UpdateStatus {}

pub fn spawn_check(cx: &mut App) {
    // Before hydration and before the config gate: an interrupted portable
    // replacement has to surface whether or not checking is on, and it is
    // recorded as a failure so `hydrate_from_disk` below carries it into the
    // UI. The scan itself is one `read_dir`; the deletions it hands back ride
    // the background sweep.
    #[cfg(target_os = "windows")]
    let finished_backups = reconcile_portable_backups();
    #[cfg(not(target_os = "windows"))]
    let finished_backups: Vec<PathBuf> = Vec::new();
    // Before the config gate: a package staged by an earlier run, or a failure
    // from one, has to reach Settings whether or not checking is still on.
    hydrate_from_disk(cx);
    // Off the startup path: this can remove a 30 MB directory, and nothing
    // waits on the result.
    let keep = UpdateState::load().pending.map(|pending| pending.stage);
    cx.background_executor()
        .spawn(async move {
            for backup in finished_backups {
                log::info!(
                    "removing a completed update's leftover backup at {}",
                    backup.display()
                );
                let _ = std::fs::remove_dir_all(&backup);
            }
            sweep_orphaned_stages(keep)
        })
        .detach();
    if !cx.global::<Config>().check_for_updates {
        return;
    }
    spawn_check_inner(false, cx);
    spawn_recheck_loop(cx);
}

pub fn spawn_check_forced(cx: &mut App) {
    spawn_check_inner(true, cx);
}

/// Republishes what the last run left on disk. Without this the UI opens
/// claiming nothing is happening while a verified package sits in staging.
fn hydrate_from_disk(cx: &mut App) {
    let mut state = UpdateState::load();
    // A package for a version we are already running is finished business —
    // most often because it is the very update that just installed itself.
    if let Some(pending) = &state.pending
        && (!pending.is_usable() || !is_update_available(&pending.version, current_version()))
    {
        let stage = pending.stage.clone();
        state.pending = None;
        state.save();
        let _ = std::fs::remove_dir_all(stage);
    }
    let (ready, failure) = (state.pending.clone(), state.last_failure.clone());
    update_status(cx, |status| {
        status.ready = ready;
        status.failure = failure;
    });
}

fn spawn_recheck_loop(cx: &mut App) {
    // No exit condition: the task is dropped with the executor when the app
    // shuts down, and the config is re-read each time so switching checking off
    // takes effect without tearing the loop down.
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(RECHECK_INTERVAL).await;
            cx.update(|cx| {
                if cx.global::<Config>().check_for_updates {
                    spawn_check_inner(false, cx);
                }
            });
        }
    })
    .detach();
}

fn spawn_check_inner(report_failure: bool, cx: &mut App) {
    if is_busy(cx) {
        return;
    }
    update_status(cx, |status| status.phase = UpdatePhase::Checking);

    let manual_proxy = cx.global::<Config>().http_proxy.clone();
    let auto_download = cx.global::<Config>().auto_download_updates;
    let channel = cx.global::<Config>().update_channel;

    cx.spawn(async move |cx| {
        let current = current_version();
        let (release, version) = match fetch_latest_release(channel, manual_proxy)
            .or(async {
                cx.background_executor().timer(CHECK_TIMEOUT).await;
                Err(anyhow::anyhow!("timed out after {CHECK_TIMEOUT:?}"))
            })
            .await
        {
            Ok(v) => v,
            Err(e) => {
                log::debug!("update check skipped: {e:#}");
                let detail = format!("{e:#}");
                cx.update(|cx| {
                    update_status(cx, |status| {
                        status.phase = if report_failure {
                            UpdatePhase::Failed(UpdateFailure::Check(detail))
                        } else {
                            UpdatePhase::Idle
                        };
                    })
                });
                return;
            }
        };

        if !is_update_available(&version, current) {
            log::debug!("update check: up to date ({channel:?} {version}, running {current})");
            cx.update(|cx| {
                update_status(cx, |status| {
                    status.available = None;
                    status.phase = UpdatePhase::UpToDate;
                })
            });
            return;
        }

        let selection = select_release_asset(&version, &release.assets);
        let available = AvailableUpdate {
            version: version.clone(),
            installable: selection.asset.is_some(),
            install_hint: selection.reason,
            #[cfg(target_os = "windows")]
            needs_elevation: selection.needs_elevation,
            asset: selection.asset,
        };
        log::info!("update available: {version} (running {current})");

        cx.update(|cx| {
            update_status(cx, |status| {
                status.available = Some(available.clone());
                status.phase = UpdatePhase::Idle;
            })
        });

        let state = UpdateState::load();
        // Fetch while the prompt is up rather than after it: the answer people
        // give depends on how long the work sounds, and by the time they have
        // read the dialog the package is usually already there.
        let staged = state.pending.as_ref().is_some_and(|p| p.version == version);
        if auto_download && available.installable && !staged {
            // Fetching ahead is not consent to install. Cleared explicitly
            // because the flag outlives one download: a package armed by an
            // earlier "Install on Next Launch" must not arm this one too.
            APPLY_ON_LAUNCH.store(false, Ordering::Relaxed);
            cx.update(|cx| spawn_download(available.clone(), cx));
        }

        if !should_prompt(&state, &version) {
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
            let mut state = UpdateState::load();
            state.last_prompted = Some(version);
            state.remind_after = None;
            state.save();
        }
    })
    .detach();
}

/// Whether this version has earned another interruption.
///
/// The rule this replaces was "prompted once, never again", which meant a
/// failed install — or a mis-click — retired the version permanently and left
/// Settings as the only place it still existed.
fn should_prompt(state: &UpdateState, version: &str) -> bool {
    if state.last_prompted.as_deref() != Some(version) {
        return true;
    }
    // Same version we already asked about: only once "Later" has expired.
    state.remind_after.is_some_and(|due| now_secs() >= due)
}

fn is_busy(cx: &App) -> bool {
    cx.try_global::<UpdateStatus>().is_some_and(|status| {
        matches!(
            status.phase,
            UpdatePhase::Checking
                | UpdatePhase::Downloading { .. }
                | UpdatePhase::Verifying
                | UpdatePhase::Installing
        )
    })
}

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn update_status(cx: &mut App, edit: impl FnOnce(&mut UpdateStatus)) {
    let mut status = cx.try_global::<UpdateStatus>().cloned().unwrap_or_default();
    edit(&mut status);
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
    let hint = update
        .install_hint
        .as_ref()
        .map(localized_update_install_hint);
    let detail = if update.installable {
        // Windows cannot replace a running daemon's image, so its install path
        // stops the background service — the promise that panes survive is
        // only true where the daemon really does keep running (macOS).
        let detail_key = if cfg!(target_os = "windows") {
            L10nKey::UpdateDialogDetailWindows
        } else {
            L10nKey::UpdateDialogDetail
        };
        let base = t_fmt(
            detail_key,
            &[
                ("version", update.version.as_str()),
                ("current", current_version()),
            ],
        );
        // The elevation warning outranks any other note: it is the one thing
        // the user must know *before* the app quits, because the next thing
        // they see is a UAC prompt (#504).
        if update.needs_elevation() {
            format!("{base} {}", t(L10nKey::UpdateDialogNeedsElevation))
        } else {
            match hint.as_deref() {
                Some(note) => format!("{base} {note}"),
                None => base,
            }
        }
    } else {
        t_fmt(
            L10nKey::UpdateDialogDetailManual,
            &[
                ("version", update.version.as_str()),
                ("current", current_version()),
                (
                    "hint",
                    hint.as_deref()
                        .unwrap_or_else(|| t(L10nKey::UpdateDialogCannotSelfUpdate)),
                ),
            ],
        )
    };
    // "Later" is the cancel answer: it takes Escape, and a closed window or a
    // dropped channel falls through to it too, so the outcome nobody chose is
    // always the one that changes nothing. Skipping a version is a decision
    // people should have to reach for — it lives in Settings, not one stray
    // keystroke away.
    let buttons: Vec<gpui::PromptButton> = if update.installable {
        let mut buttons = vec![gpui::PromptButton::ok(t(
            L10nKey::SettingsUpdateAndRelaunch,
        ))];
        // An unattended next-launch install is a promise an elevation-needing
        // package cannot keep: nobody is there to answer UAC before the first
        // window (#504). The only honest offer is "now".
        if !update.needs_elevation() {
            buttons.push(gpui::PromptButton::ok(t(L10nKey::UpdateDialogNextLaunch)));
        }
        buttons.push(gpui::PromptButton::cancel(t(L10nKey::UpdateDialogLater)));
        buttons
    } else {
        vec![
            gpui::PromptButton::ok(t(L10nKey::SettingsUpdateViewRelease)),
            gpui::PromptButton::cancel(t(L10nKey::UpdateDialogLater)),
        ]
    };
    let answer = window.prompt(
        PromptLevel::Info,
        t(L10nKey::UpdateDialogTitle),
        Some(&detail),
        &buttons,
        cx,
    );
    let update = update.clone();
    cx.spawn(async move |cx| {
        let installable = update.installable;
        // With elevation in play, button 1 *is* "Later" — only a layout that
        // offered "next launch" may treat index 1 as that answer.
        let offered_next_launch = installable && !update.needs_elevation();
        match answer.await {
            Ok(0) if installable => {
                cx.update(install_available);
            }
            Ok(0) => open_releases_page(),
            Ok(1) if offered_next_launch => {
                cx.update(|cx| stage_for_next_launch(update, cx));
            }
            // "Later" — index 1 or 2 depending on the shape — plus a dropped
            // channel and a closed window. All of them change nothing.
            _ => remind_later(),
        }
    })
    .detach();
}

/// Pushes this version's next prompt out by `REMIND_LATER` instead of retiring
/// it. Any background download already under way is left alone: the package
/// being ready costs nothing and turns the next prompt into one keystroke.
fn remind_later() {
    let mut state = UpdateState::load();
    state.remind_after = Some(now_secs() + REMIND_LATER.as_secs());
    state.save();
}

/// Drops everything the previous channel produced, then checks the new feed.
///
/// A staged package and a deferred prompt are both answers to a question the
/// old feed asked; neither carries over. The staged package especially — one
/// armed for the next launch would otherwise install a Stable build onto
/// someone who just moved to Nightly.
///
/// "Everything" includes a download still in flight, which is the likely one:
/// checking starts a background transfer on its own, so the seconds spent
/// finding this setting are seconds that transfer is running. Cancelling stops
/// it where it can be stopped, and the generation bump covers the rest.
///
/// Nightly to Stable deliberately does *not* downgrade. The nightly in hand
/// keeps running until a stable release supersedes it, which is already how
/// `parse_version` orders a release above the prerelease sharing its core
/// version. Rolling back to an older build to honour the switch immediately
/// would be a bigger surprise than arriving there one release later.
pub fn switch_channel(cx: &mut App) {
    CHANNEL_GENERATION.fetch_add(1, Ordering::Relaxed);
    cancel_download(cx);
    discard_pending(cx);
    let mut state = UpdateState::load();
    state.last_prompted = None;
    state.remind_after = None;
    state.last_failure = None;
    state.save();
    update_status(cx, |status| {
        status.available = None;
        status.failure = None;
        status.phase = UpdatePhase::Idle;
    });
    spawn_check_forced(cx);
}

/// Abandons the transfer. The staging directory is removed by the download
/// thread as it unwinds, so nothing is left to collect.
pub fn cancel_download(cx: &mut App) {
    DOWNLOAD_CANCELLED.store(true, Ordering::Relaxed);
    INSTALL_WHEN_READY.store(false, Ordering::Relaxed);
    APPLY_ON_LAUNCH.store(false, Ordering::Relaxed);
    update_status(cx, |status| status.phase = UpdatePhase::Idle);
}

/// Install as soon as there is something to install: now if the package is
/// already staged, otherwise when the download in flight finishes.
pub fn install_available(cx: &mut App) {
    let status = cx.try_global::<UpdateStatus>().cloned().unwrap_or_default();
    if let Some(pending) = status.ready.clone()
        && pending.is_usable()
        && is_update_available(&pending.version, current_version())
    {
        launch_pending(pending, cx);
        return;
    }
    let Some(update) = status.available.clone() else {
        return;
    };
    if !update.installable {
        open_releases_page();
        return;
    }
    INSTALL_WHEN_READY.store(true, Ordering::Relaxed);
    // Asking to install now overrides an earlier "at next launch": whatever
    // lands is being handed straight to the updater.
    APPLY_ON_LAUNCH.store(false, Ordering::Relaxed);
    // A check with auto-download on has usually started one already; joining it
    // beats running a second copy of the same transfer.
    if !matches!(
        status.phase,
        UpdatePhase::Downloading { .. } | UpdatePhase::Verifying
    ) {
        spawn_download(update, cx);
    }
}

/// Have the package waiting, and let the next launch apply it unattended. This
/// is the option that costs the user nothing: no interrupted work now, and no
/// decision to make later either.
pub fn stage_for_next_launch(update: AvailableUpdate, cx: &mut App) {
    #[cfg(target_os = "windows")]
    if update.needs_elevation {
        // "Next launch" promises an unattended install, which an
        // elevation-needing package cannot keep — there is nobody to answer a
        // UAC prompt before the first window exists (#504). Offer the one
        // path that works instead: install now, with the user present.
        install_available(cx);
        return;
    }
    APPLY_ON_LAUNCH.store(true, Ordering::Relaxed);
    // Overrides a pending "install as soon as it lands": choosing next launch
    // is choosing not to be restarted now.
    INSTALL_WHEN_READY.store(false, Ordering::Relaxed);
    let status = cx.try_global::<UpdateStatus>().cloned().unwrap_or_default();
    if let Some(pending) = status
        .ready
        .clone()
        .filter(|pending| pending.version == update.version && pending.is_usable())
    {
        arm_pending(pending, cx);
        return;
    }
    if !matches!(
        status.phase,
        UpdatePhase::Downloading { .. } | UpdatePhase::Verifying
    ) {
        spawn_download(update, cx);
    }
}

/// Marks an already-staged package for unattended install at next launch.
fn arm_pending(mut pending: PendingUpdate, cx: &mut App) {
    pending.apply_on_launch = true;
    let mut state = UpdateState::load();
    state.pending = Some(pending.clone());
    state.save();
    update_status(cx, |status| status.ready = Some(pending));
}

/// Throws away a staged package and its staging directory.
pub fn discard_pending(cx: &mut App) {
    let mut state = UpdateState::load();
    if let Some(pending) = state.pending.take() {
        let _ = std::fs::remove_dir_all(&pending.stage);
    }
    state.save();
    update_status(cx, |status| status.ready = None);
}

/// Clears the recorded failure once the user has seen it.
pub fn dismiss_failure(cx: &mut App) {
    let mut state = UpdateState::load();
    state.last_failure = None;
    state.save();
    update_status(cx, |status| {
        status.failure = None;
        if matches!(status.phase, UpdatePhase::Failed(_)) {
            status.phase = UpdatePhase::Idle;
        }
    });
}

fn spawn_download(update: AvailableUpdate, cx: &mut App) {
    if matches!(
        cx.try_global::<UpdateStatus>().map(|status| &status.phase),
        Some(UpdatePhase::Downloading { .. } | UpdatePhase::Verifying | UpdatePhase::Installing)
    ) {
        return;
    }
    let Some(asset) = update.asset.clone() else {
        open_releases_page();
        return;
    };

    DOWNLOAD_CANCELLED.store(false, Ordering::Relaxed);
    DOWNLOAD_VERIFYING.store(false, Ordering::Relaxed);
    DOWNLOAD_RECEIVED.store(0, Ordering::Relaxed);
    DOWNLOAD_TOTAL.store(0, Ordering::Relaxed);
    update_status(cx, |status| {
        status.available = Some(update.clone());
        status.failure = None;
        status.phase = UpdatePhase::Downloading {
            received: 0,
            total: None,
        };
    });
    spawn_progress_pump(cx);

    let generation = CHANNEL_GENERATION.load(Ordering::Relaxed);
    let version = update.version.clone();
    let task = cx.background_executor().spawn(smol::unblock(move || {
        prepare_update(&version, &asset, &|received, total| {
            DOWNLOAD_RECEIVED.store(received, Ordering::Relaxed);
            DOWNLOAD_TOTAL.store(total.unwrap_or(0), Ordering::Relaxed);
            if DOWNLOAD_CANCELLED.load(Ordering::Relaxed) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
    }));

    cx.spawn(async move |cx| {
        let prepared = match task.await {
            Ok(prepared) => prepared,
            Err(error) => {
                // Both flags are consent to *this* attempt. Leaving either set
                // would hand the next background download — one the user never
                // asked for — a licence to restart the app under them.
                INSTALL_WHEN_READY.store(false, Ordering::Relaxed);
                APPLY_ON_LAUNCH.store(false, Ordering::Relaxed);
                if DOWNLOAD_CANCELLED.load(Ordering::Relaxed) {
                    log::info!("update download cancelled");
                    // Only if nothing has claimed the phase since. A channel
                    // switch cancels this download and starts a check in the
                    // same breath, and the check is already the live work by
                    // the time the transfer notices it was called off.
                    cx.update(|cx| {
                        update_status(cx, |status| {
                            if matches!(
                                status.phase,
                                UpdatePhase::Downloading { .. } | UpdatePhase::Verifying
                            ) {
                                status.phase = UpdatePhase::Idle;
                            }
                        })
                    });
                    return;
                }
                let detail = format!("{error:#}");
                log::error!("update failed: {detail}");
                cx.update(|cx| {
                    record_failure(&update.version, &detail, cx);
                    update_status(cx, |status| {
                        status.phase = UpdatePhase::Failed(UpdateFailure::Prepare(detail));
                    });
                });
                return;
            }
        };

        let pending = prepared.into_pending(
            update.version.clone(),
            APPLY_ON_LAUNCH.load(Ordering::Relaxed),
        );
        // The feed moved under this download. Staging it anyway would hand a
        // Nightly user the Stable package they were mid-way through fetching
        // when they left — and if they had pressed install, relaunch them into
        // it. Both flags were consent to a version from the old channel.
        if CHANNEL_GENERATION.load(Ordering::Relaxed) != generation {
            log::info!(
                "discarding {}: the update channel changed while it was being prepared",
                update.version
            );
            INSTALL_WHEN_READY.store(false, Ordering::Relaxed);
            APPLY_ON_LAUNCH.store(false, Ordering::Relaxed);
            let _ = std::fs::remove_dir_all(&pending.stage);
            return;
        }
        let mut state = UpdateState::load();
        state.pending = Some(pending.clone());
        state.last_failure = None;
        state.save();
        cx.update(|cx| {
            update_status(cx, |status| {
                status.ready = Some(pending.clone());
                status.failure = None;
                status.phase = UpdatePhase::Idle;
            });
            if INSTALL_WHEN_READY.swap(false, Ordering::Relaxed) {
                launch_pending(pending, cx);
            }
        });
    })
    .detach();
}

/// Samples the download counters into the global the UI renders from. Stops as
/// soon as the phase leaves the transfer, so a finished, cancelled or failed
/// download does not leave a timer running.
fn spawn_progress_pump(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(PROGRESS_TICK).await;
            let live = cx.update(|cx| {
                let received = DOWNLOAD_RECEIVED.load(Ordering::Relaxed);
                let total = DOWNLOAD_TOTAL.load(Ordering::Relaxed);
                let verifying = DOWNLOAD_VERIFYING.load(Ordering::Relaxed);
                let mut live = false;
                update_status(cx, |status| {
                    if matches!(
                        status.phase,
                        UpdatePhase::Downloading { .. } | UpdatePhase::Verifying
                    ) {
                        status.phase = if verifying {
                            UpdatePhase::Verifying
                        } else {
                            UpdatePhase::Downloading {
                                received,
                                total: (total > 0).then_some(total),
                            }
                        };
                        live = true;
                    }
                });
                live
            });
            if !live {
                return;
            }
        }
    })
    .detach();
}

fn launch_pending(pending: PendingUpdate, cx: &mut App) {
    update_status(cx, |status| status.phase = UpdatePhase::Installing);
    #[cfg(target_os = "windows")]
    if pending.needs_elevation {
        return launch_pending_elevated(pending, cx);
    }
    match pending.launch() {
        Ok(()) => {
            // The updater owns the staging directory from here. Forgetting it
            // now is what stops a relaunch from trying to install it twice.
            let mut state = UpdateState::load();
            state.pending = None;
            state.last_prompted = None;
            state.save();
            cx.quit();
        }
        Err(error) => {
            let detail = format!("{error:#}");
            log::error!("could not start the installer: {detail}");
            record_failure(&pending.version, &detail, cx);
            update_status(cx, |status| {
                status.phase = UpdatePhase::Failed(UpdateFailure::Launch(detail));
            });
        }
    }
}

/// Starts the elevated install chain (#504): a de-elevated watcher that will
/// relaunch the app, then a single UAC prompt covering the two privileged
/// stages.
///
/// Success looks exactly like a plain install from here — this process quits
/// and the watcher, not the GUI, owns the relaunch. Declining the UAC prompt
/// is not a failure: nothing ever ran elevated, nothing needs recording, and
/// the staged package is still usable, so the plan stays put and the app
/// stays up. Settings still shows it as ready to install.
#[cfg(target_os = "windows")]
fn launch_pending_elevated(pending: PendingUpdate, cx: &mut App) {
    // ShellExecuteEx cannot run on the UI thread: raising the UAC prompt
    // pumps this thread's message loop (the shell broadcasts change
    // notifications through the window), which re-enters gpui while its App
    // is borrowed and aborts the process on a RefCell double-borrow. The
    // whole launch — watcher spawn included, so the pairing stays atomic —
    // runs on a background thread; only the bookkeeping comes back.
    let launch = cx.background_executor().spawn(smol::unblock(move || {
        let version = pending.version.clone();
        (pending.launch_elevated(), version)
    }));
    cx.spawn(async move |cx| {
        let (result, version) = launch.await;
        cx.update(|cx| match result {
            Ok(ElevationStart::Started) => {
                // The watcher owns the staging directory from here. Forgetting
                // it now is what stops a relaunch from trying to install it
                // twice.
                let mut state = UpdateState::load();
                state.pending = None;
                state.last_prompted = None;
                state.save();
                cx.quit();
            }
            Ok(ElevationStart::Declined) => {
                log::info!(
                    "administrator approval for the {version} update was declined; \
                     the package stays staged"
                );
                update_status(cx, |status| status.phase = UpdatePhase::Idle);
            }
            Err(error) => {
                let detail = format!("{error:#}");
                log::error!("could not start the elevated install: {detail}");
                record_failure(&version, &detail, cx);
                update_status(cx, |status| {
                    status.phase = UpdatePhase::Failed(UpdateFailure::Launch(detail));
                });
            }
        })
    })
    .detach();
}

/// Records a failure where the user can still find it tomorrow, and lets the
/// version prompt again.
///
/// The old code wrote `last_prompted` the moment a dialog appeared and never
/// looked back, so an install that failed a second later took the version with
/// it: never prompted again, and the only trace was a phase that died with the
/// process.
fn record_failure(version: &str, detail: &str, cx: &mut App) {
    let record = FailureRecord {
        version: version.to_string(),
        detail: detail.to_string(),
    };
    let mut state = UpdateState::load();
    state.last_failure = Some(record.clone());
    if state.last_prompted.as_deref() == Some(version) {
        state.last_prompted = None;
        state.remind_after = None;
    }
    state.save();
    update_status(cx, |status| status.failure = Some(record));
}

/// Opens the page for whichever channel this installation follows. A Nightly
/// user sent to the Stable release page would be handed the wrong package —
/// and, since Linux and unsupported installs update by hand, that page is the
/// entire update path for some of them.
///
/// Reads the config from disk rather than the global: several callers reach
/// here from a background thread where the `App` is out of reach.
pub fn open_releases_page() {
    open_url(match Config::load().update_channel {
        UpdateChannel::Stable => RELEASES_URL,
        UpdateChannel::Nightly => NIGHTLY_RELEASE_URL,
    });
}

pub fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener).arg(url).spawn() {
        log::warn!("failed to open {url}: {e}");
    }
}

/// Installs a package the user asked to have applied at the next launch, and
/// reports whether this process should now get out of the way.
///
/// This is what makes "Install on Next Launch" worth offering: the user never
/// waits for a download and never answers a second question — they quit tty7
/// one evening and start a new version the next morning.
///
/// Must run before the daemon is contacted and before any window exists.
/// Spawning a daemon this process is about to abandon costs a restart, and a
/// window that appears and vanishes reads as a crash.
///
/// Every failure path clears the plan and returns `false`. An update that
/// cannot be applied must never become an app that will not start.
pub fn apply_pending_at_launch() -> bool {
    let mut state = UpdateState::load();
    let Some(pending) = state.pending.clone() else {
        return false;
    };
    if !pending.apply_on_launch {
        return false;
    }
    if !pending.is_usable() || !is_update_available(&pending.version, current_version()) {
        log::info!(
            "discarding the staged {} update: no longer applicable to {}",
            pending.version,
            current_version()
        );
        state.pending = None;
        state.save();
        let _ = std::fs::remove_dir_all(&pending.stage);
        return false;
    }

    #[cfg(target_os = "windows")]
    if pending.needs_elevation {
        // An unattended start has nobody to approve a UAC prompt, and raising
        // one before the first window — unsigned path and all — reads as
        // malware. The plan stays staged; once the app is up, Settings offers
        // the install with the window behind it (#504).
        log::info!(
            "the staged {} update needs administrator approval; leaving it for an interactive install",
            pending.version
        );
        return false;
    }

    log::info!(
        "applying the staged {} update before startup",
        pending.version
    );
    // Cleared before the handover rather than after: the updater takes the
    // staging directory with it, so a plan left on disk would be retried at the
    // next launch against a directory that no longer exists.
    state.pending = None;
    state.last_prompted = None;
    state.remind_after = None;
    state.save();

    match pending.launch() {
        Ok(()) => true,
        Err(error) => {
            let detail = format!("{error:#}");
            log::error!("could not apply the staged update: {detail}");
            let mut state = UpdateState::load();
            state.last_failure = Some(FailureRecord {
                version: pending.version.clone(),
                detail,
            });
            state.save();
            false
        }
    }
}

/// Folds the outcome the updater recorded for the install attempt that
/// produced this launch into the on-disk update state, then removes the file.
///
/// This is what a failed install used to lack (#540): the GUI quits as soon
/// as the helper is spawned, so without the outcome file a failure lived only
/// in `update.log` — and because launching the helper had already cleared the
/// prompt state, the next check simply offered the same version again. Runs
/// before any window exists; `spawn_check`'s hydration carries the result
/// into Settings.
pub fn absorb_update_outcome_at_launch() {
    use tty7_core::daemon::install::outcome::{UpdateOutcome, read_outcome};

    let Some(path) = update_outcome_path() else {
        return;
    };
    let outcome = match read_outcome(&path) {
        Ok(Some(outcome)) => outcome,
        Ok(None) => return,
        // A result that exists but cannot be read is itself a result: an
        // updater ran, and what it left is unusable.
        Err(error) => UpdateOutcome {
            version: current_version().to_string(),
            ok: false,
            detail: Some(format!(
                "the update result at {} could not be read: {error}",
                path.display()
            )),
        },
    };
    // Consumed either way: an outcome describes the attempt that already ran,
    // never the next one.
    let _ = std::fs::remove_file(&path);

    let mut state = UpdateState::load();
    if outcome.ok {
        if outcome.version == current_version() {
            log::info!("the update to {} completed", outcome.version);
            // A failure recorded by an earlier attempt at this same version
            // is finished business now.
            if state
                .last_failure
                .as_ref()
                .is_some_and(|failure| failure.version == outcome.version)
            {
                state.last_failure = None;
                state.save();
            }
        } else {
            // The helper said the install went in, yet this process is a
            // different version — someone reinstalled by hand in between.
            // Nothing to show, but the log should have it.
            log::warn!(
                "the updater reported installing {} but this is {}; ignoring the stale outcome",
                outcome.version,
                current_version()
            );
        }
        return;
    }

    let detail = outcome
        .detail
        .unwrap_or_else(|| "the update failed without recording a reason".to_string());
    log::error!("the update to {} failed: {detail}", outcome.version);
    state.last_failure = Some(FailureRecord {
        version: outcome.version.clone(),
        detail,
    });
    // A failure must not retire the version — `last_prompted` set with no
    // `remind_after` is "one failed install retires it for good", the
    // invariant `a_failure_lets_the_version_prompt_again` pins. But this
    // attempt already restarted the app once, and `spawn_check` runs at every
    // launch: asking again seconds later is the nag loop #540 is about. The
    // middle course is the one "Later" already uses — keep the version marked
    // as asked, and push the next ask out by `REMIND_LATER`. The failure sits
    // in Settings the whole while.
    state.last_prompted = Some(outcome.version);
    state.remind_after = Some(now_secs() + REMIND_LATER.as_secs());
    state.save();
}

/// Removes staging directories belonging to a run that died before its updater
/// could clean up — quitting mid-download is the usual cause.
///
/// Worth doing: on macOS staging is created beside the app bundle, which is
/// normally /Applications, and a hidden 30 MB directory there is one nobody
/// finds on purpose. The live plan's own directory is kept however old it is.
fn sweep_orphaned_stages(keep: Option<PathBuf>) {
    for root in stage_roots() {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if keep.as_deref() == Some(path.as_path()) {
                continue;
            }
            if !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_stage_name)
            {
                continue;
            }
            let expired = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > STAGE_TTL);
            if expired {
                log::info!("removing orphaned update staging at {}", path.display());
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
}

/// Where each platform's staging directories are created — beside the bundle
/// on macOS and beside the image on Linux (both have to be on the installed
/// file's own volume to rename into place), and the per-user temp directory
/// on Windows.
// The `return`s are what let one cfg block win per platform; clippy sees only
// the surviving one and reads it as redundant.
#[allow(clippy::needless_return)]
fn stage_roots() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        return current_macos_app_bundle()
            .and_then(|app| app.parent().map(Path::to_path_buf))
            .into_iter()
            .collect();
    }
    #[cfg(target_os = "linux")]
    {
        return current_appimage()
            .and_then(|appimage| appimage.parent().map(Path::to_path_buf))
            .into_iter()
            .collect();
    }
    #[cfg(target_os = "windows")]
    {
        return vec![std::env::temp_dir()];
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    Vec::new()
}

/// The prefixes `update_staging_dir` and `system_update_staging_dir` hand to
/// `tempfile`.
fn is_stage_name(name: &str) -> bool {
    name.starts_with(".tty7-update-") || name.starts_with("tty7-update-")
}

/// Deals with `.tty7-update-backup-*` directories a previous portable update
/// left in the installation.
///
/// One still carrying the incomplete marker means the replacement was cut
/// short — power loss, a kill — and the installed files may mix two versions.
/// That is recorded as an update failure so Settings shows it (and shows it
/// again at every launch until the user restores or deletes the backup: the
/// warning describes a condition, not an event). The backup itself is kept —
/// it holds the only copy of the previous files.
///
/// One without the marker is a completed update whose backup deletion lost to
/// an antivirus scan; those are returned for the background sweep to remove.
#[cfg(target_os = "windows")]
fn reconcile_portable_backups() -> Vec<PathBuf> {
    let Some(WindowsUpdateLayout::Portable(dir)) = current_windows_update_layout() else {
        return Vec::new();
    };
    let (interrupted, finished) = scan_portable_backups(&dir);
    if !interrupted.is_empty() {
        // All of them, not the first: two interrupted attempts in a row leave
        // two backups, and one the user never hears about is one they delete
        // blind or keep forever.
        let preserved = interrupted
            .iter()
            .map(|backup| backup.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let detail = format!(
            "a previous update was interrupted while replacing the installed files, so {} may \
             mix two versions; the files from before are preserved at {preserved} — restore \
             them or reinstall, then delete the backup",
            dir.display()
        );
        log::warn!("{detail}");
        let mut state = UpdateState::load();
        if state.last_failure.as_ref().map(|failure| &failure.detail) != Some(&detail) {
            state.last_failure = Some(FailureRecord {
                version: current_version().to_string(),
                detail,
            });
            state.save();
        }
    }
    finished
}

/// Splits the backups under `dir` into (interrupted, finished) by the
/// incomplete marker. Pure directory inspection, so the policy above stays
/// testable without a real installation.
#[cfg(target_os = "windows")]
fn scan_portable_backups(dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut interrupted = Vec::new();
    let mut finished = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (interrupted, finished);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let named = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(WINDOWS_PORTABLE_BACKUP_PREFIX));
        if !named || !path.is_dir() {
            continue;
        }
        if path.join(WINDOWS_PORTABLE_BACKUP_INCOMPLETE).is_file() {
            interrupted.push(path);
        } else {
            finished.push(path);
        }
    }
    (interrupted, finished)
}

pub(crate) fn localized_update_phase(phase: &UpdatePhase) -> Option<String> {
    match phase {
        UpdatePhase::Idle => None,
        UpdatePhase::Checking => Some(t(L10nKey::SettingsUpdateChecking).to_string()),
        UpdatePhase::UpToDate => Some(t(L10nKey::SettingsUpdateUpToDate).to_string()),
        UpdatePhase::Downloading { received, total } => Some(match total {
            // A percentage needs both ends; GitHub occasionally serves the
            // asset without a usable content-length, and "42%" of an unknown
            // whole is worse than an honest byte count.
            Some(total) if *total > 0 => t_fmt(
                L10nKey::SettingsUpdateDownloadingPercent,
                &[
                    (
                        "percent",
                        &(received.saturating_mul(100) / total).min(100).to_string(),
                    ),
                    ("size", &human_bytes(*total)),
                ],
            ),
            _ => t_fmt(
                L10nKey::SettingsUpdateDownloadingBytes,
                &[("received", &human_bytes(*received))],
            ),
        }),
        UpdatePhase::Verifying => Some(t(L10nKey::SettingsUpdateVerifying).to_string()),
        UpdatePhase::Installing => Some(t(L10nKey::SettingsUpdateInstalling).to_string()),
        UpdatePhase::Failed(failure) => {
            let (key, error) = match failure {
                UpdateFailure::Check(error) => (L10nKey::SettingsUpdateCheckFailed, error),
                UpdateFailure::Prepare(error) => (L10nKey::SettingsUpdatePrepareFailed, error),
                UpdateFailure::Launch(error) => (L10nKey::SettingsUpdateLaunchFailed, error),
            };
            Some(t_fmt(key, &[("error", error)]))
        }
    }
}

pub(crate) fn localized_update_install_hint(hint: &UpdateInstallHint) -> String {
    match hint {
        #[cfg(target_os = "macos")]
        UpdateInstallHint::UnsupportedMacos => {
            t(L10nKey::SettingsUpdateUnsupportedMacos).to_string()
        }
        #[cfg(target_os = "linux")]
        UpdateInstallHint::UnsupportedLinux => {
            t(L10nKey::SettingsUpdateUnsupportedLinux).to_string()
        }
        #[cfg(target_os = "linux")]
        UpdateInstallHint::LinuxManualPackage(name) => {
            t_fmt(L10nKey::SettingsUpdateLinuxPackage, &[("name", name)])
        }
        #[cfg(target_os = "windows")]
        UpdateInstallHint::UnsupportedWindows => {
            t(L10nKey::SettingsUpdateUnsupportedWindows).to_string()
        }
        #[cfg(target_os = "windows")]
        UpdateInstallHint::WindowsAllUsersInstall => {
            t(L10nKey::SettingsUpdateWindowsAllUsers).to_string()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        UpdateInstallHint::UnsupportedPlatform => {
            t(L10nKey::SettingsUpdateUnsupportedPlatform).to_string()
        }
        UpdateInstallHint::MissingPackage(name) => {
            t_fmt(L10nKey::SettingsUpdateMissingPackage, &[("name", name)])
        }
        UpdateInstallHint::MissingChecksums => {
            t(L10nKey::SettingsUpdateMissingChecksums).to_string()
        }
    }
}

fn human_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    format!("{:.1} MB", bytes as f64 / MB)
}

/// The shape of a persisted [`PendingUpdate`]. A plan written by a build
/// whose updater invocation differs from this one's is discarded rather than
/// launched: its staged helper is the *old* build's updater and would not
/// understand this build's arguments. Absent from pre-parameterization
/// plans, which deserialize as 0 and never match.
const PLAN_VERSION: u32 = 2;

/// A downloaded, verified package waiting to be installed.
///
/// Splitting "fetch" from "install" is the point of the whole design: it turns
/// the question put to the user from "will you spend five minutes on this now"
/// into "may I restart", which is the difference between an update that gets
/// applied and one that gets postponed indefinitely.
///
/// The parent pid is *not* stored. It is the one argument that cannot survive a
/// restart, so `launch` supplies it fresh — see `PreparedUpdate::args`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingUpdate {
    pub version: String,
    /// Whether the next launch installs this without asking. False for a
    /// package that was merely fetched ahead of time: it waits in Settings.
    #[serde(default)]
    pub apply_on_launch: bool,
    /// See [`PLAN_VERSION`].
    #[serde(default)]
    plan_version: u32,
    updater: PathBuf,
    command: String,
    rest: Vec<PathBuf>,
    config_dir: Option<PathBuf>,
    stage: PathBuf,
    /// Windows, all-users Inno layout: install through the elevated chain
    /// (#504) rather than by spawning the staged helper directly.
    #[serde(default)]
    needs_elevation: bool,
    /// The staged package's digest as the release server published it. The
    /// elevated chain's trust anchor: the checksums file beside the package
    /// cannot serve there, because a medium-integrity process can rewrite
    /// both together.
    #[serde(default)]
    expected_sha256: Option<String>,
}

impl PendingUpdate {
    /// Whether the package is still on disk and still speaks this build's
    /// updater protocol. Staging lives in a temporary directory that a
    /// cleaner, an antivirus, or a reboot may have taken; the plan version
    /// is the other half — see [`PLAN_VERSION`].
    pub fn is_usable(&self) -> bool {
        self.plan_version == PLAN_VERSION && self.updater.is_file() && self.stage.is_dir()
    }

    fn launch(&self) -> Result<()> {
        PreparedUpdate {
            updater: self.updater.clone(),
            command: self.command.clone(),
            rest: self.rest.clone(),
            config_dir: self.config_dir.clone(),
            stage: self.stage.clone(),
            needs_elevation: self.needs_elevation,
            expected_sha256: self.expected_sha256.clone(),
        }
        .launch()
    }

    /// The pieces of a staged Inno plan that the elevated launch needs, read
    /// out of `rest` by position — the order `prepare_windows_update` pushes
    /// them. A plan that cannot name every piece is not elevated.
    #[cfg(target_os = "windows")]
    fn elevated_parts(&self) -> Option<ElevatedPlanParts> {
        if !self.needs_elevation || self.command != "install" || self.rest.len() != 7 {
            return None;
        }
        Some(ElevatedPlanParts {
            installer: self.rest[0].clone(),
            asset_name: self.rest[2].to_str()?.to_string(),
            install_dir: self.rest[3].clone(),
            version: self.rest[4].to_str()?.to_string(),
            log: self.rest[5].clone(),
            stage: self.rest[6].clone(),
            expected_sha256: self.expected_sha256.clone()?,
        })
    }
}

/// See [`PendingUpdate::elevated_parts`].
#[cfg(target_os = "windows")]
struct ElevatedPlanParts {
    installer: PathBuf,
    asset_name: String,
    install_dir: PathBuf,
    version: String,
    log: PathBuf,
    stage: PathBuf,
    expected_sha256: String,
}

/// How asking Windows to start the privileged half ended. Only `Started`
/// hands the machine to elevated processes; `Declined` means the user
/// answered the UAC prompt with "no" and nothing ever ran elevated.
#[cfg(target_os = "windows")]
enum ElevationStart {
    Started,
    Declined,
}

#[cfg(target_os = "windows")]
impl PendingUpdate {
    /// Spawns the de-elevated relaunch watcher, then raises the single UAC
    /// prompt that covers both privileged stages (#504).
    ///
    /// The watcher goes first so a declined prompt leaves exactly one
    /// medium-integrity process to reap. The elevated half is always the
    /// *installed* updater — the trust root a medium-integrity process cannot
    /// rewrite — and everything it needs arrives as arguments, because an
    /// elevated child (over-the-shoulder especially) does not inherit this
    /// process's environment.
    fn launch_elevated(&self) -> Result<ElevationStart> {
        use windows_sys::Win32::Foundation::ERROR_CANCELLED;

        let mut parts = self
            .elevated_parts()
            .context("the staged package does not describe an elevated install")?;
        // The prompt names the installation this process is running from, not
        // the one the plan names. `update.json` lives in the user's config
        // directory: a plan naming some other directory would aim a UAC
        // prompt — and the elevated half's `<install-dir>` — at a binary of
        // the plan writer's choosing.
        let installed_updater =
            bundled_updater().context("tty7-updater.exe is not installed beside this app")?;
        let installed_dir = installed_updater
            .parent()
            .context("the installed updater names no directory")?
            .to_path_buf();
        parts.install_dir = installed_dir.clone();
        let Some(status) = update_elevation_status_path() else {
            anyhow::bail!("no config directory for the elevation status file");
        };
        // The watcher requires it: without the outcome file the install's
        // result would die with the elevated chain (#540).
        let Some(outcome) = update_outcome_path() else {
            anyhow::bail!("no config directory for the install outcome file");
        };
        let app = std::env::current_exe().context("locating the running app")?;
        // Stale markers from an earlier attempt have to be gone: the watcher
        // reads both files, and would otherwise act on a dead attempt's
        // answer.
        let _ = std::fs::remove_file(&status);
        let _ = std::fs::remove_file(&outcome);

        let mut watcher = Command::new(&self.updater);
        watcher
            .arg("relaunch-watcher")
            .arg("--status-file")
            .arg(&status)
            .arg("--result-file")
            .arg(&outcome)
            .arg("--app-path")
            .arg(&app)
            .arg("--log")
            .arg(&parts.log)
            .arg("--expected-version")
            .arg(&parts.version)
            // So the watcher can tell a GUI that is quitting from one that
            // stayed: a declined prompt leaves this process on screen, and
            // the relaunch it would otherwise perform on its own timeout
            // would put a second window beside it.
            .arg("--gui-pid")
            .arg(std::process::id().to_string());
        if let Some(dir) = &self.config_dir {
            watcher.arg("--config-dir").arg(dir);
        }
        let mut watcher = tty7_core::core::proc::hide_console(&mut watcher)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("launching the relaunch watcher")?;

        let parameters =
            elevated_stage_arguments(&parts, &status, &outcome, self.config_dir.as_deref());
        match shell_execute_elevated(&installed_updater, &parameters, &installed_dir) {
            Ok(()) => Ok(ElevationStart::Started),
            Err(code) => {
                // Whatever the answer was, the watcher must not outlive this
                // process when no elevated half is coming: it would spend its
                // whole 15-minute grace waiting for a chain that will never
                // report, while the app it was to relaunch is still running.
                let _ = watcher.kill();
                let _ = std::fs::remove_file(&status);
                if code == ERROR_CANCELLED {
                    return Ok(ElevationStart::Declined);
                }
                Err(anyhow::anyhow!(
                    "asking Windows to run the install elevated failed (error {code})"
                ))
            }
        }
    }
}

/// The command line for the privileged first stage, as a single string:
/// ShellExecuteEx hands it to the child's CRT argv splitter verbatim, so
/// every argument is quoted — the install directory typically lives under
/// `C:\Program Files`. Built as an `OsString` so a path that is not valid
/// Unicode survives the round trip.
#[cfg(target_os = "windows")]
fn elevated_stage_arguments(
    parts: &ElevatedPlanParts,
    status_file: &Path,
    outcome: &Path,
    config_dir: Option<&Path>,
) -> std::ffi::OsString {
    let mut line = std::ffi::OsString::new();
    let mut push = |arg: &std::ffi::OsStr| push_quoted(&mut line, arg);
    push(std::ffi::OsStr::new("elevated-stage"));
    push(std::ffi::OsStr::new(&std::process::id().to_string()));
    push(parts.installer.as_os_str());
    push(std::ffi::OsStr::new(&parts.asset_name));
    push(parts.install_dir.as_os_str());
    push(std::ffi::OsStr::new(&parts.version));
    push(parts.log.as_os_str());
    push(parts.stage.as_os_str());
    push(std::ffi::OsStr::new("--expected-sha256"));
    push(std::ffi::OsStr::new(&parts.expected_sha256));
    push(std::ffi::OsStr::new("--status-file"));
    push(status_file.as_os_str());
    if let Some(dir) = config_dir {
        push(std::ffi::OsStr::new("--config-dir"));
        push(dir.as_os_str());
    }
    push(std::ffi::OsStr::new("--result-file"));
    push(outcome.as_os_str());
    line
}

/// Appends one argument the way `CommandLineToArgvW` — and the CRT splitter
/// the elevated helper is parsed by — reads back verbatim.
///
/// Wrapping in quotes is not enough on its own. A backslash is an escape
/// character only in front of a quote, so a path that *ends* in one
/// (`--config-dir "D:\tty7\"`) escapes its own closing quote and swallows the
/// rest of the command line into a single argument; an embedded quote splits
/// one argument into several. Neither can be reached across the integrity
/// boundary — every value here comes from this user's own session — but the
/// receiver runs elevated, and an argument list it can misread is not a thing
/// to leave to the shape of a path.
#[cfg(target_os = "windows")]
fn push_quoted(line: &mut std::ffi::OsString, arg: &std::ffi::OsStr) {
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};

    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;

    let mut quoted = vec![QUOTE];
    // The run of backslashes immediately behind the cursor: doubled if a
    // quote follows it, left alone otherwise.
    let mut backslashes = 0usize;
    for unit in arg.encode_wide() {
        match unit {
            BACKSLASH => {
                backslashes += 1;
                quoted.push(unit);
            }
            QUOTE => {
                quoted.resize(quoted.len() + backslashes, BACKSLASH);
                quoted.push(BACKSLASH);
                quoted.push(QUOTE);
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                quoted.push(unit);
            }
        }
    }
    // The closing quote is a quote like any other: the run in front of it has
    // to be doubled too.
    quoted.resize(quoted.len() + backslashes, BACKSLASH);
    quoted.push(QUOTE);

    if !line.is_empty() {
        line.push(" ");
    }
    line.push(std::ffi::OsString::from_wide(&quoted));
}

/// Starts `executable` elevated — the `runas` verb is what turns the request
/// into exactly one UAC prompt — and returns as soon as Windows answers. The
/// elevated process is deliberately not tracked by handle: the status file
/// and the watcher own liveness from here, which is what lets this process
/// quit immediately.
///
/// The error case carries the raw Windows error code so the caller can tell
/// "the user said no" (`ERROR_CANCELLED`) apart from a genuine launch
/// failure.
#[cfg(target_os = "windows")]
fn shell_execute_elevated(
    executable: &Path,
    parameters: &std::ffi::OsStr,
    working_dir: &Path,
) -> std::result::Result<(), u32> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::UI::Shell::{SEE_MASK_FLAG_NO_UI, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    let verb = wide(std::ffi::OsStr::new("runas"));
    let file = wide(executable.as_os_str());
    let params = wide(parameters);
    let directory = wide(working_dir.as_os_str());
    // SAFETY: zero-initializing is valid for this all-POD struct; every field
    // that matters is set below.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    // No error dialog from the shell itself: a decline is a normal answer the
    // caller handles, and a real failure is reported in-process.
    info.fMask = SEE_MASK_FLAG_NO_UI;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.lpDirectory = directory.as_ptr();
    // The updater is a console program; an elevated console window flashing
    // up beside the GUI reads as a crash.
    info.nShow = SW_HIDE;
    // SAFETY: `info` is fully initialized, and every pointer names a
    // NUL-terminated buffer that outlives the call.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        // SAFETY: called immediately after the failed call on the same
        // thread, so the error code is the one ShellExecuteExW set.
        return Err(unsafe { GetLastError() });
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailureRecord {
    pub version: String,
    pub detail: String,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct UpdateState {
    /// The version a dialog has already been shown for. Paired with
    /// `remind_after`, which decides when that stops counting.
    #[serde(default)]
    last_prompted: Option<String>,
    /// Unix seconds after which `last_prompted` may prompt again.
    #[serde(default)]
    remind_after: Option<u64>,
    #[serde(default)]
    last_failure: Option<FailureRecord>,
    #[serde(default)]
    pending: Option<PendingUpdate>,
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

/// `nightly.json`, written by the nightly workflow. Only `version` is read
/// today; the rest is there so a build can be traced back to its commit
/// without cross-referencing the release notes.
#[derive(Clone, Debug, serde::Deserialize)]
struct NightlyManifest {
    version: String,
    #[serde(default)]
    #[allow(dead_code)]
    commit: String,
    #[serde(default)]
    #[allow(dead_code)]
    published_at: String,
}

/// The GitHub endpoint each channel reads.
///
/// Keeping the two feeds apart is the whole point of the channel: neither can
/// hand the other an update, so an installation only changes channel when the
/// user changes it in Settings.
fn release_endpoint(channel: UpdateChannel) -> String {
    match channel {
        // Excludes prereleases by definition, so Stable can never be offered a
        // nightly even though both live in the same repository.
        UpdateChannel::Stable => format!("https://api.github.com/repos/{REPO}/releases/latest"),
        UpdateChannel::Nightly => {
            format!("https://api.github.com/repos/{REPO}/releases/tags/{NIGHTLY_TAG}")
        }
    }
}

/// Recovers the version from a package name such as
/// `tty7-26.8.2-nightly.202608071800-macos-arm64.zip`.
///
/// The platform segment is a closed set, which is what makes the split
/// unambiguous — the version is everything between the `tty7-` prefix and the
/// platform marker. `tty7-server-linux-x86_64-musl` matches that shape too but
/// yields `server`, which `parse_version` rejects, so the remote-server assets
/// sitting in the same release are skipped without special-casing them.
///
/// The highest version wins rather than the first one found. Two nights can be
/// on the release at once: the workflow uploads tonight's packages before
/// pruning yesterday's, and a prune that never ran leaves them there for good.
/// GitHub lists assets oldest first, so taking the first match is taking the
/// older build — which reads as "no update" and stalls the channel silently.
fn version_from_assets(assets: &[GitHubAsset]) -> Option<String> {
    assets
        .iter()
        .filter_map(|asset| {
            let rest = asset.name.strip_prefix("tty7-")?;
            let cut = ["-macos-", "-linux-", "-windows-"]
                .iter()
                .find_map(|marker| rest.find(marker))?;
            let version = &rest[..cut];
            Some((parse_version(version)?, version.to_string()))
        })
        .max()
        .map(|(_, version)| version)
}

fn build_http_client(manual_proxy: Option<&str>) -> Result<ReqwestClient> {
    let user_agent = concat!("tty7/", env!("CARGO_PKG_VERSION"));
    // Normalise through the same helper the downloader uses, so a bare
    // `127.0.0.1:7890` proxies the check as well as the download.
    if let Some(proxy) = manual_proxy
        .and_then(tty7_core::daemon::install::proxy::normalize_manual)
        .and_then(|url| http_client::Url::parse(&url).ok())
    {
        ReqwestClient::proxy_and_user_agent(Some(proxy), user_agent).context("building HTTP client")
    } else {
        ReqwestClient::user_agent(user_agent).context("building HTTP client")
    }
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    client: &ReqwestClient,
    url: &str,
) -> Result<T> {
    let request = http_client::Request::get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .follow_redirects(RedirectPolicy::FollowAll)
        .body(AsyncBody::default())
        .context("building request")?;

    let mut response = client.send(request).await.context("sending the request")?;

    if !response.status().is_success() {
        anyhow::bail!("GitHub returned HTTP {}", response.status().as_u16());
    }

    let mut body = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut body)
        .await
        .context("reading response body")?;

    serde_json::from_slice(&body).context("parsing JSON")
}

/// Returns the release together with the version it advertises, which is not
/// always something the release object states outright — see `resolve_version`.
async fn fetch_latest_release(
    channel: UpdateChannel,
    manual_proxy: Option<String>,
) -> Result<(LatestRelease, String)> {
    let client = build_http_client(manual_proxy.as_deref())?;
    let release: LatestRelease = fetch_json(&client, &release_endpoint(channel))
        .await
        .context("requesting the release")?;
    let version = resolve_version(&client, &release)
        .await
        .with_context(|| format!("release {} advertises no usable version", release.tag_name))?;
    Ok((release, version))
}

/// The version a release stands for.
///
/// Stable states it in the tag (`v26.8.1`) and needs nothing else. Nightly
/// cannot: its tag is force-moved to a new commit every night and so is the
/// literal string `nightly`, which carries no version at all. It publishes
/// `nightly.json` beside the packages instead.
///
/// The fall back to asset names covers the two cases where the manifest is
/// absent — a nightly published before it existed, and a night where writing it
/// failed — because neither is a reason to strand the whole channel.
async fn resolve_version(client: &ReqwestClient, release: &LatestRelease) -> Option<String> {
    if parse_version(&release.tag_name).is_some() {
        return Some(release.tag_name.trim_start_matches('v').to_string());
    }
    if let Some(asset) = release
        .assets
        .iter()
        .find(|asset| asset.name == NIGHTLY_MANIFEST)
    {
        match fetch_json::<NightlyManifest>(client, &asset.browser_download_url).await {
            Ok(manifest) if parse_version(&manifest.version).is_some() => {
                return Some(manifest.version);
            }
            Ok(manifest) => log::warn!(
                "{NIGHTLY_MANIFEST} declares an unusable version {:?}; falling back to asset names",
                manifest.version
            ),
            Err(e) => log::warn!("could not read {NIGHTLY_MANIFEST}: {e:#}"),
        }
    }
    version_from_assets(&release.assets)
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
    #[cfg(target_os = "windows")]
    needs_elevation: bool,
}

fn select_release_asset(version: &str, assets: &[GitHubAsset]) -> AssetSelection {
    select_release_asset_for(package_for_current_install(version), assets)
}

fn select_release_asset_for(
    package: Result<PackageOffer, UpdateInstallHint>,
    assets: &[GitHubAsset],
) -> AssetSelection {
    let offer = match package {
        Ok(offer) => offer,
        Err(reason) => {
            return AssetSelection {
                asset: None,
                reason: Some(reason),
                #[cfg(target_os = "windows")]
                needs_elevation: false,
            };
        }
    };
    let name = offer.name;
    let Some(asset) = assets.iter().find(|asset| asset.name == name) else {
        return AssetSelection {
            asset: None,
            reason: Some(UpdateInstallHint::MissingPackage(name)),
            #[cfg(target_os = "windows")]
            needs_elevation: false,
        };
    };
    let Some(checksums) = assets.iter().find(|asset| asset.name == "checksums.txt") else {
        return AssetSelection {
            asset: None,
            reason: Some(UpdateInstallHint::MissingChecksums),
            #[cfg(target_os = "windows")]
            needs_elevation: false,
        };
    };
    AssetSelection {
        asset: Some(ReleaseAsset {
            name,
            url: asset.browser_download_url.clone(),
            checksums_url: checksums.browser_download_url.clone(),
        }),
        reason: None,
        #[cfg(target_os = "windows")]
        needs_elevation: offer.needs_elevation,
    }
}

/// The release package this installation can replace itself with. Split from
/// the bare filename so the Windows Inno layout can carry "yes, but the
/// install needs a UAC prompt" alongside it (#504).
#[derive(Debug)]
struct PackageOffer {
    name: String,
    #[cfg(target_os = "windows")]
    needs_elevation: bool,
}

impl PackageOffer {
    fn plain(name: String) -> Self {
        Self {
            name,
            #[cfg(target_os = "windows")]
            needs_elevation: false,
        }
    }
}

/// The release package this installation can replace itself with, or the
/// reason it cannot.
// One `#[cfg]` block per platform, each answering the whole question. Every
// block `return`s rather than trailing off, because only one of them survives
// preprocessing and *which* one is last is a property of the target, not of the
// source: dropping the keyword compiles wherever that block happens to land at
// the end and breaks on every other target. clippy only ever sees the one block
// this host builds, so it reads the keyword as redundant.
#[allow(clippy::needless_return)]
fn package_for_current_install(version: &str) -> Result<PackageOffer, UpdateInstallHint> {
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
        return Ok(PackageOffer::plain(format!(
            "tty7-{version}-macos-{arch}.zip"
        )));
    }
    #[cfg(target_os = "linux")]
    {
        let arch = if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else {
            // The release publishes no Linux package for anything else, so
            // there is no filename worth naming.
            return Err(UpdateInstallHint::UnsupportedLinux);
        };
        let appimage = current_appimage();
        // An AppImage can replace itself only if a staging directory fits
        // beside it — the swap is two renames, which need one filesystem —
        // and only if it carries the helper that performs the swap after
        // this process exits. Images from before the helper shipped fail the
        // second check and keep the manual path they always had.
        let can_self_update = appimage.as_deref().is_some_and(|appimage| {
            is_appimage_update_writable(appimage) && bundled_updater().is_some()
        });
        return linux_package_for(version, arch, appimage.is_some(), can_self_update);
    }
    #[cfg(target_os = "windows")]
    {
        let Some(layout) = current_windows_update_layout() else {
            return Err(UpdateInstallHint::UnsupportedWindows);
        };
        let needs_elevation = match windows_layout_updatability(&layout) {
            WindowsUpdatability::Updatable => false,
            WindowsUpdatability::NeedsElevation => true,
            WindowsUpdatability::Unsupported(hint) => return Err(hint),
        };
        if !layout.directory().join("tty7-updater.exe").is_file() {
            return Err(UpdateInstallHint::UnsupportedWindows);
        }
        // UAC is pointed at the *installed* updater, so the elevated chain is
        // only as real as the verbs that binary speaks: one from a release
        // before the verbs existed keeps today's answer — the release page.
        if needs_elevation && !updater_speaks_elevation(layout.directory()) {
            return Err(UpdateInstallHint::WindowsAllUsersInstall);
        }
        return windows_package_for_layout(version, &layout)
            .map(|name| PackageOffer {
                name,
                needs_elevation,
            })
            .ok_or(UpdateInstallHint::UnsupportedWindows);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = version;
        Err(UpdateInstallHint::UnsupportedPlatform)
    }
}

/// Whether the updater installed beside this app speaks the elevation verbs
/// (`elevated-stage` / `install-elevated` / `relaunch-watcher`). Probed by
/// execution rather than version comparison: the capability list is the
/// contract, and a manually downgraded or side-loaded binary answers for
/// itself. One short-lived process per check; an updater that predates the
/// verb exits with its usage error, which is the same "no".
#[cfg(target_os = "windows")]
fn updater_speaks_elevation(install_dir: &Path) -> bool {
    let updater = install_dir.join("tty7-updater.exe");
    let mut command = Command::new(&updater);
    command
        .arg("capabilities")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let output = tty7_core::core::proc::hide_console(&mut command).output();
    let Ok(output) = output else { return false };
    output.status.success() && capabilities_cover_elevation(&output.stdout)
}

/// The pure half, so the token matching is testable without spawning a real
/// helper. Mirrors `ELEVATION_CAPABILITIES` in the updater.
#[cfg(target_os = "windows")]
fn capabilities_cover_elevation(stdout: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(stdout) else {
        return false;
    };
    let tokens: std::collections::HashSet<&str> = text.lines().map(str::trim).collect();
    ["elevated-stage", "install-elevated", "relaunch-watcher"]
        .iter()
        .all(|capability| tokens.contains(capability))
}

// Per-platform `#[cfg]` blocks, each `return`ing for the reason spelled out
// over `package_for_current_install`.
#[allow(clippy::needless_return)]
fn prepare_update(
    version: &str,
    asset: &ReleaseAsset,
    on_progress: &dyn Fn(u64, Option<u64>) -> ControlFlow<()>,
) -> Result<PreparedUpdate> {
    // Runs off the main thread, so the `Config` global is out of reach.
    let cfg = Config::load();
    let fetcher =
        tty7_core::daemon::install::download::HttpsFetcher::new(cfg.http_proxy.as_deref());
    // Fetched without progress: a few hundred bytes next to a 30 MB package.
    let checksums = fetcher
        .get(&asset.checksums_url)
        .map_err(anyhow::Error::msg)
        .context("downloading checksums.txt")?;
    let archive = fetcher
        .get_cancellable(&asset.url, on_progress)
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("downloading {}", asset.name))?;
    DOWNLOAD_VERIFYING.store(true, Ordering::Relaxed);
    #[cfg(target_os = "macos")]
    {
        return prepare_macos_update(version, &asset.name, &archive, &checksums);
    }
    #[cfg(target_os = "windows")]
    {
        return prepare_windows_update(version, &asset.name, &archive, &checksums);
    }
    #[cfg(target_os = "linux")]
    {
        return prepare_linux_update(version, &asset.name, &archive, &checksums);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    anyhow::bail!("automatic installation is not supported on this platform")
}

#[derive(Debug)]
struct PreparedUpdate {
    updater: PathBuf,
    /// The updater subcommand: `install`, or `install-portable` on Windows.
    command: String,
    /// Every argument after the parent pid.
    rest: Vec<PathBuf>,
    config_dir: Option<PathBuf>,
    stage: PathBuf,
    /// Windows, all-users Inno layout: launch through the elevated chain
    /// (#504), not by spawning the staged helper directly.
    needs_elevation: bool,
    /// See [`PendingUpdate::expected_sha256`].
    expected_sha256: Option<String>,
}

impl PreparedUpdate {
    /// The pid is filled in here rather than baked into `rest`, so a plan
    /// written to disk today still names the right process when a launch
    /// tomorrow runs it.
    fn args(&self) -> Vec<PathBuf> {
        let mut args = Vec::with_capacity(self.rest.len() + 2);
        args.push(PathBuf::from(&self.command));
        args.push(std::process::id().to_string().into());
        args.extend(self.rest.iter().cloned());
        args
    }

    fn into_pending(self, version: String, apply_on_launch: bool) -> PendingUpdate {
        PendingUpdate {
            version,
            apply_on_launch,
            plan_version: PLAN_VERSION,
            updater: self.updater,
            command: self.command,
            rest: self.rest,
            config_dir: self.config_dir,
            stage: self.stage,
            needs_elevation: self.needs_elevation,
            expected_sha256: self.expected_sha256,
        }
    }

    fn launch(&self) -> Result<()> {
        let mut command = Command::new(&self.updater);
        command.args(self.args());
        let outcome = update_outcome_path();
        for arg in updater_tail_args(self.config_dir.as_deref(), outcome.as_deref()) {
            command.arg(arg);
        }
        if let Some(outcome) = &outcome {
            // A result from an earlier attempt has to be gone before this one
            // starts: a helper that dies before writing its own would
            // otherwise be read as having written *that* one.
            let _ = std::fs::remove_file(outcome);
        }
        tty7_core::core::proc::hide_console(&mut command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&self.stage);
            })
            .context("launching tty7-updater")?;
        Ok(())
    }
}

/// Where the updater records how the install ended; the next launch merges it
/// into the update state (`absorb_update_outcome_at_launch`).
fn update_outcome_path() -> Option<PathBuf> {
    crate::core::config::config_path(tty7_core::daemon::install::outcome::OUTCOME_FILE_NAME)
}

/// The file the privileged first stage writes its pid into so the de-elevated
/// watcher can tell the chain apart from a UAC prompt nobody has answered
/// yet. It cannot live in the privileged staging directory (the watcher's
/// token may not read there), so it sits next to the outcome file: an
/// administrator can write the user's config directory under every elevation
/// shape, including over-the-shoulder (#504).
#[cfg(target_os = "windows")]
fn update_elevation_status_path() -> Option<PathBuf> {
    crate::core::config::config_path("update-elevation.status")
}

/// The named options after the positional arguments. Everything the updater
/// must know about this process's configuration crosses as arguments, never
/// the environment: on Windows an elevated (UAC) child does not inherit the
/// spawning process's environment, so a `TTY7_CONFIG_DIR` set there would
/// fall back to the administrator's config directory exactly in the case
/// that needs the caller's (#504). The updater re-exports the variable for
/// the children it spawns itself.
fn updater_tail_args(config_dir: Option<&Path>, outcome: Option<&Path>) -> Vec<std::ffi::OsString> {
    let mut args = Vec::new();
    if let Some(dir) = config_dir {
        args.push(std::ffi::OsString::from("--config-dir"));
        args.push(dir.as_os_str().to_os_string());
    }
    if let Some(outcome) = outcome {
        args.push(std::ffi::OsString::from("--result-file"));
        args.push(outcome.as_os_str().to_os_string());
    }
    args
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
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
        command: "install".to_string(),
        rest: vec![
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
        needs_elevation: false,
        expected_sha256: None,
    })
}

/// Stages a downloaded AppImage beside the installed one and has the bundled
/// updater verify it while this process is still around to show a failure.
///
/// Staging lives in the image's own directory for the same reason macOS
/// stages beside the bundle: the swap is two renames, and renames only stay
/// atomic on one filesystem. The helper that performs them is *copied* into
/// staging rather than run from the image — the image is a FUSE mount the
/// runtime tears down when the GUI exits, which is precisely the moment the
/// installer starts working.
#[cfg(target_os = "linux")]
fn prepare_linux_update(
    version: &str,
    asset_name: &str,
    archive: &[u8],
    checksums: &[u8],
) -> Result<PreparedUpdate> {
    let current = current_appimage().context("tty7 is not running from an AppImage")?;
    let parent = current
        .parent()
        .context("the AppImage has no parent directory")?;
    let bundled = bundled_updater().context("tty7-updater is not bundled with this image")?;
    let staging = update_staging_dir(parent)?;
    let dir = staging.path().to_path_buf();
    let archive = write_staged_asset(&dir, asset_name, archive)?;
    let checksums = write_staged_asset(&dir, "checksums.txt", checksums)?;
    run_updater(
        &bundled,
        [
            PathBuf::from("verify"),
            archive.clone(),
            checksums.clone(),
            PathBuf::from(asset_name),
            dir.clone(),
            PathBuf::from(version),
        ],
    )?;
    let updater = dir.join("tty7-updater");
    std::fs::copy(&bundled, &updater)
        .with_context(|| format!("copying the updater to {}", updater.display()))?;
    let log =
        crate::core::config::config_path("update.log").unwrap_or_else(|| dir.join("update.log"));
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).context("creating the update log directory")?;
    }
    let dir = staging.keep();
    Ok(PreparedUpdate {
        updater,
        command: "install".to_string(),
        rest: vec![
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
        needs_elevation: false,
        expected_sha256: None,
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
    let updatability = windows_layout_updatability(&layout);
    let needs_elevation = match updatability {
        WindowsUpdatability::Updatable => false,
        WindowsUpdatability::NeedsElevation => true,
        // This error surfaces in Settings as the update failure — the moment
        // the user most needs to understand it — so it is the localized hint,
        // not the English one meant for logs (#602).
        WindowsUpdatability::Unsupported(hint) => {
            anyhow::bail!("{}", localized_update_install_hint(&hint))
        }
    };
    let install_dir = layout.directory().to_path_buf();
    let bundled = bundled_updater().context("tty7-updater.exe is not bundled with this app")?;

    // The digest the elevated chain trusts, taken from the downloaded
    // manifest rather than the staged copy of it: the staged file lands in a
    // user-writable directory, so it cannot anchor its own verification.
    // Computed for both Inno modes — a plan's elevation answer is re-derived
    // at launch, and the digest costs one manifest parse.
    let expected_sha256 = if matches!(layout, WindowsUpdateLayout::Inno(_)) {
        let manifest =
            std::str::from_utf8(checksums).context("checksums.txt is not valid UTF-8")?;
        Some(
            tty7_core::daemon::install::checksums::expected_digest(manifest, asset_name)
                .map(|digest| tty7_core::daemon::install::checksums::hex(&digest))
                .context("reading the staged package's digest from checksums.txt")?,
        )
    } else {
        None
    };

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
        command: install_command.to_string(),
        rest: vec![
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
        needs_elevation,
        expected_sha256,
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

/// The updater shipped inside the mounted image, beside this executable at
/// `usr/bin`. Resolved through `current_exe` rather than `$APPDIR` so a
/// stale or hand-set variable cannot point the update machinery at a binary
/// that is not the one this process actually runs beside.
#[cfg(target_os = "linux")]
fn bundled_updater() -> Option<PathBuf> {
    let updater = std::env::current_exe().ok()?.parent()?.join("tty7-updater");
    updater.is_file().then_some(updater)
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn bundled_updater() -> Option<PathBuf> {
    None
}

/// The image this process is running from. The AppImage runtime exports
/// `$APPIMAGE`, pointing at the file it mounted; nothing else identifies
/// which of the Linux packages is installed. Required to name an existing
/// absolute path — the variable outlives moves and deletions, and every
/// answer given here is a file the updater will later rename.
#[cfg(target_os = "linux")]
fn current_appimage() -> Option<PathBuf> {
    let appimage = PathBuf::from(std::env::var_os("APPIMAGE")?);
    (appimage.is_absolute() && appimage.is_file()).then_some(appimage)
}

#[cfg(target_os = "linux")]
fn is_appimage_update_writable(appimage: &Path) -> bool {
    appimage.parent().is_some_and(can_stage_replacement_in)
}

/// The Linux answer, split from the probing so the policy is testable
/// without an AppImage runtime setting variables. Only an AppImage that can
/// stage and swap itself is offered an install; every other shape keeps the
/// named-package hint it always had — a tarball unpacks wherever the user
/// chose, a distro package belongs to its package manager, and guessing at
/// either is exactly what `package_for_current_install` must never do.
#[cfg(target_os = "linux")]
fn linux_package_for(
    version: &str,
    arch: &str,
    is_appimage: bool,
    can_self_update: bool,
) -> Result<PackageOffer, UpdateInstallHint> {
    if !is_appimage {
        return Err(UpdateInstallHint::LinuxManualPackage(format!(
            "tty7-{version}-linux-{arch}.tar.gz"
        )));
    }
    let name = format!("tty7-{version}-linux-{arch}.AppImage");
    if can_self_update {
        Ok(PackageOffer::plain(name))
    } else {
        Err(UpdateInstallHint::LinuxManualPackage(name))
    }
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

/// What this Windows installation layout means for an in-place update.
#[cfg(target_os = "windows")]
#[derive(Clone, Debug, PartialEq, Eq)]
enum WindowsUpdatability {
    /// A per-user Inno install, or a writable portable directory: the
    /// updater runs as the signed-in user, no prompt involved.
    Updatable,
    /// An Inno install replacing it needs administrator rights for (#504):
    /// the elevated chain handles it — one announced UAC prompt, and the app
    /// never runs elevated itself.
    NeedsElevation,
    Unsupported(UpdateInstallHint),
}

/// Sorts the Windows installation layouts by what replacing them takes,
/// before anything is downloaded.
#[cfg(target_os = "windows")]
fn windows_layout_updatability(layout: &WindowsUpdateLayout) -> WindowsUpdatability {
    match layout {
        WindowsUpdateLayout::Inno(directory) => {
            if windows_inno_needs_elevation(directory) {
                return WindowsUpdatability::NeedsElevation;
            }
            WindowsUpdatability::Updatable
        }
        WindowsUpdateLayout::Portable(directory) => {
            if !windows_directory_is_writable(directory) {
                return WindowsUpdatability::Unsupported(UpdateInstallHint::UnsupportedWindows);
            }
            WindowsUpdatability::Updatable
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
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

/// `(major, minor, patch, is_release, build)`.
///
/// Ordering the release flag *before* the build number is what lets a stable
/// release supersede the prerelease that carries the same core version: a
/// Nightly stamped `26.7.1-nightly.202607161800` is offered `v26.7.1` and
/// graduates out of the prerelease no matter how recent its build is.
///
/// `build` then orders two prereleases sharing a core version against each
/// other, by the timestamp the nightly workflow stamps. That comparison is the
/// whole reason the Nightly channel can roll forward at all — without it every
/// nightly compares equal to the next one. It is unreachable on Stable, which
/// reads `/releases/latest` and so never sees a prerelease.
///
/// Every numeric identifier in the prerelease is collected, not just the last
/// one, and they are compared left to right the way semver compares them. The
/// nightly stamp is a single number today, but reading only the last segment
/// meant that appending one — a counter for a second build in the same day, a
/// finer clock — would silently *reverse* the ordering (`…20260807.2` scoring
/// 2) instead of failing where someone would notice. Non-numeric identifiers
/// are skipped rather than ranked: `-beta` and `-rc` are not versions this
/// project ships, and guessing an order between them buys nothing. A
/// prerelease with no number at all yields an empty list, which sorts below
/// every stamped build.
fn parse_version(s: &str) -> Option<(u64, u64, u64, bool, Vec<u64>)> {
    let trimmed = s.trim();
    let core = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let without_meta = core.split('+').next().unwrap_or(core);
    let is_release = !without_meta.contains('-');
    let build = without_meta
        .split_once('-')
        .map(|(_, pre)| pre.split('.').filter_map(|id| id.parse().ok()).collect())
        .unwrap_or_default();
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch, is_release, build))
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
        let selected = select_release_asset_for(Ok(PackageOffer::plain(name.to_string())), &assets);
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
        let selected = select_release_asset_for(
            Ok(PackageOffer::plain(name.to_string())),
            &[github_asset(name)],
        );
        assert!(selected.asset.is_none());
        assert_eq!(selected.reason, Some(UpdateInstallHint::MissingChecksums));
    }

    #[test]
    fn release_without_the_exact_platform_package_is_never_guessed() {
        let selected = select_release_asset_for(
            Ok(PackageOffer::plain(
                "tty7-27.1.0-macos-arm64.zip".to_string(),
            )),
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

    /// The one Linux shape that installs itself: an AppImage whose directory
    /// takes a staging directory and whose image carries the helper.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_appimage_that_can_replace_itself_is_offered_the_appimage() {
        let offer = linux_package_for("27.1.0", "x86_64", true, true)
            .expect("a self-updating AppImage yields an offer");
        assert_eq!(offer.name, "tty7-27.1.0-linux-x86_64.AppImage");
    }

    /// Everything else keeps the manual hint, and the hint names the exact
    /// package for the installation shape rather than pointing at a release
    /// page with five files on it.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_installs_that_cannot_self_update_name_their_package() {
        // An AppImage in a read-only directory, or one from before the
        // helper shipped: still an AppImage, still updated by hand.
        assert_eq!(
            linux_package_for("27.1.0", "x86_64", true, false).unwrap_err(),
            UpdateInstallHint::LinuxManualPackage("tty7-27.1.0-linux-x86_64.AppImage".to_string())
        );
        // A tarball (or distro-packaged) install is never guessed at.
        assert_eq!(
            linux_package_for("27.1.0", "x86_64", false, false).unwrap_err(),
            UpdateInstallHint::LinuxManualPackage("tty7-27.1.0-linux-x86_64.tar.gz".to_string())
        );
        // `can_self_update` without an AppImage cannot happen (the probe is
        // gated on the variable), but the policy must not invent an offer if
        // it ever does.
        assert!(linux_package_for("27.1.0", "x86_64", false, true).is_err());
    }

    /// The offered AppImage name must match what the release actually
    /// publishes, checksums manifest included — the same end-to-end shape the
    /// macOS selection test pins.
    #[cfg(target_os = "linux")]
    #[test]
    fn appimage_selection_matches_the_published_asset_names() {
        let name = "tty7-27.1.0-linux-x86_64.AppImage";
        let offer = linux_package_for("27.1.0", "x86_64", true, true).unwrap();
        let assets = [
            github_asset("tty7-27.1.0-linux-x86_64.tar.gz"),
            github_asset(name),
            github_asset("checksums.txt"),
        ];
        let selected = select_release_asset_for(Ok(offer), &assets);
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

    #[cfg(target_os = "windows")]
    #[test]
    fn portable_backup_scan_separates_interrupted_from_finished() {
        let root = tempfile::tempdir().unwrap();
        let interrupted = root.path().join(".tty7-update-backup-cut");
        std::fs::create_dir(&interrupted).unwrap();
        std::fs::write(interrupted.join(WINDOWS_PORTABLE_BACKUP_INCOMPLETE), b"").unwrap();
        let finished = root.path().join(".tty7-update-backup-done");
        std::fs::create_dir(&finished).unwrap();
        // Neither a user's directory nor a stray file may be touched.
        std::fs::create_dir(root.path().join("completions")).unwrap();
        std::fs::write(root.path().join(".tty7-update-backup-not-a-dir"), b"file").unwrap();

        let (got_interrupted, got_finished) = scan_portable_backups(root.path());
        assert_eq!(got_interrupted, vec![interrupted]);
        assert_eq!(got_finished, vec![finished]);
    }

    #[test]
    fn parses_versions_with_and_without_prefix() {
        let release = |major, minor, patch| Some((major, minor, patch, true, vec![]));
        assert_eq!(parse_version("v0.3.1"), release(0, 3, 1));
        assert_eq!(parse_version("0.3.1"), release(0, 3, 1));
        assert_eq!(parse_version(" 1.2.0 "), release(1, 2, 0));
        assert_eq!(parse_version("v2"), release(2, 0, 0));
        assert_eq!(parse_version("v2.5"), release(2, 5, 0));
        assert_eq!(
            parse_version("v0.4.0-rc.1"),
            Some((0, 4, 0, false, vec![1]))
        );
        assert_eq!(
            parse_version("26.7.1-nightly.202607161800"),
            Some((26, 7, 1, false, vec![202607161800]))
        );
        // A prerelease with nothing numeric to order by still parses; it just
        // sorts below every stamped build of the same core version.
        assert_eq!(parse_version("1.0.0-beta"), Some((1, 0, 0, false, vec![])));
        assert_eq!(parse_version("0.4.0+ci.7"), release(0, 4, 0));
        assert_eq!(parse_version("nightly"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("v0.3.1.1"), None);
        assert_eq!(parse_version("0.3.1.0"), None);
        assert_eq!(parse_version("vv0.3.1"), None);
    }

    /// Every numeric identifier is collected, so appending one to the nightly
    /// stamp extends the ordering instead of replacing it. Reading only the
    /// last segment made `…20260807.2` score 2 and lose to the build before it.
    #[test]
    fn prerelease_identifiers_order_left_to_right() {
        assert_eq!(
            parse_version("26.8.2-nightly.20260807.2"),
            Some((26, 8, 2, false, vec![20260807, 2]))
        );
        assert!(is_update_available(
            "26.8.2-nightly.20260807.2",
            "26.8.2-nightly.20260807"
        ));
        assert!(!is_update_available(
            "26.8.2-nightly.20260807",
            "26.8.2-nightly.20260807.2"
        ));
        // The day still dominates whatever trails it.
        assert!(is_update_available(
            "26.8.2-nightly.20260808",
            "26.8.2-nightly.20260807.9"
        ));
        // And the move from date to minute stamps carries the installs that
        // are already out there: 202608071800 > 20260807.
        assert!(is_update_available(
            "26.8.2-nightly.202608071800",
            "26.8.2-nightly.20260807"
        ));
    }

    /// Two builds in one day used to collide on the date and read as "up to
    /// date" — the reason the stamp goes to the minute.
    #[test]
    fn two_builds_on_one_day_are_distinguishable() {
        assert!(is_update_available(
            "26.8.2-nightly.202608071800",
            "26.8.2-nightly.202608070200"
        ));
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
    }

    /// The Nightly channel's whole reason to exist: last night's build has to
    /// be able to supersede the one before it. This is what the old ordering
    /// deliberately refused to do, back when the only feed was
    /// `/releases/latest` and no nightly could ever be offered.
    #[test]
    fn a_newer_nightly_supersedes_an_older_one() {
        assert!(is_update_available(
            "26.7.1-nightly.20260717",
            "26.7.1-nightly.20260716"
        ));
        assert!(!is_update_available(
            "26.7.1-nightly.20260716",
            "26.7.1-nightly.20260717"
        ));
        assert!(!is_update_available(
            "26.7.1-nightly.20260716",
            "26.7.1-nightly.20260716"
        ));
        // Across core versions the date is irrelevant — a nightly for the next
        // patch wins however old its build is.
        assert!(is_update_available(
            "26.7.2-nightly.20260101",
            "26.7.1-nightly.20260716"
        ));
    }

    /// Ordering the release flag ahead of the build number is what keeps this
    /// true: a stable release outranks every dated build sharing its core
    /// version, so a user who switches back to Stable still graduates.
    #[test]
    fn a_stable_release_still_outranks_every_nightly_of_its_core_version() {
        for date in ["20260101", "20991231"] {
            assert!(is_update_available(
                "v26.7.1",
                &format!("26.7.1-nightly.{date}")
            ));
        }
    }

    /// Each channel reads its own release, which is the mechanism that keeps a
    /// Nightly from being walked back onto Stable by an update it never asked
    /// for. `/releases/latest` excludes prereleases by definition, so the two
    /// feeds cannot see each other's builds.
    #[test]
    fn each_channel_reads_its_own_feed() {
        assert!(release_endpoint(UpdateChannel::Stable).ends_with("/releases/latest"));
        assert!(
            release_endpoint(UpdateChannel::Nightly)
                .ends_with(&format!("/releases/tags/{NIGHTLY_TAG}"))
        );
        // The page a Nightly user is sent to has to be the release that feed
        // reads. `concat!` cannot build the URL from the constant, so this is
        // where the two are held together.
        assert!(NIGHTLY_RELEASE_URL.ends_with(&format!("/releases/tag/{NIGHTLY_TAG}")));
    }

    /// The fallback for a nightly published without `nightly.json`. The tag is
    /// the literal "nightly", so the asset names are the only place left that
    /// states which build this is.
    #[test]
    fn version_is_recovered_from_nightly_asset_names() {
        let assets = [
            github_asset("checksums.txt"),
            github_asset("tty7-26.8.2-nightly.20260807-macos-arm64.zip"),
            github_asset("tty7-26.8.2-nightly.20260807-windows-x86_64-setup.exe"),
        ];
        assert_eq!(
            version_from_assets(&assets).as_deref(),
            Some("26.8.2-nightly.20260807")
        );
    }

    /// The remote-server binaries ride along in the same release and match the
    /// `tty7-…-linux-…` shape, but have no version in front of the platform.
    /// `parse_version` rejecting "server" is what skips them, so no name-based
    /// special case is needed.
    #[test]
    fn remote_server_assets_are_not_mistaken_for_a_version() {
        let assets = [
            github_asset("checksums.txt"),
            github_asset("tty7-server-linux-x86_64-musl"),
            github_asset("tty7-server-linux-aarch64-musl"),
        ];
        assert_eq!(version_from_assets(&assets), None);

        // And they must not win when a real package is also present, whatever
        // order GitHub returns them in.
        let mixed = [
            github_asset("tty7-server-linux-x86_64-musl"),
            github_asset("tty7-26.8.2-nightly.202608071800-linux-x86_64.tar.gz"),
        ];
        assert_eq!(
            version_from_assets(&mixed).as_deref(),
            Some("26.8.2-nightly.202608071800")
        );
    }

    /// Tonight's packages are uploaded before last night's are pruned, so both
    /// nights are briefly on the release at once — and stay that way for good
    /// if the prune step ever fails. GitHub lists assets oldest first, so
    /// taking the first match would take yesterday's build and read as "no
    /// update", stalling the channel with no error anywhere.
    #[test]
    fn the_newest_asset_version_wins_when_two_nights_overlap() {
        let assets = [
            github_asset("tty7-26.8.2-nightly.202608062200-macos-arm64.zip"),
            github_asset("tty7-26.8.2-nightly.202608062200-linux-x86_64.tar.gz"),
            github_asset("checksums.txt"),
            github_asset("tty7-26.8.2-nightly.202608071800-macos-arm64.zip"),
            github_asset("tty7-26.8.2-nightly.202608071800-linux-x86_64.tar.gz"),
        ];
        assert_eq!(
            version_from_assets(&assets).as_deref(),
            Some("26.8.2-nightly.202608071800")
        );
    }

    /// Pins the contract between `nightly.yml`'s manifest step and this side of
    /// it. Renaming a field in the workflow silently drops Nightly back to
    /// guessing versions out of filenames; this fails instead.
    #[test]
    fn nightly_manifest_matches_what_the_workflow_writes() {
        let manifest: NightlyManifest = serde_json::from_str(
            r#"{
                "version": "26.8.2-nightly.20260807",
                "commit": "0123456789abcdef0123456789abcdef01234567",
                "published_at": "2026-08-07T02:11:00Z"
            }"#,
        )
        .expect("the workflow's shape must deserialize");
        assert_eq!(manifest.version, "26.8.2-nightly.20260807");
        assert!(parse_version(&manifest.version).is_some());

        // Older manifests, or a workflow that stops writing the extras, must
        // still yield a usable version rather than failing the whole check.
        let minimal: NightlyManifest =
            serde_json::from_str(r#"{"version": "26.8.2-nightly.20260807"}"#)
                .expect("version alone is enough");
        assert_eq!(minimal.commit, "");
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
        let _lock = UPDATE_STATE_LOCK.lock().unwrap();
        crate::core::config::pin_test_config_dir();
        let path = UpdateState::path().expect("config dir pinned");

        let _ = std::fs::remove_file(&path);
        assert_eq!(UpdateState::load().last_prompted, None);

        UpdateState {
            last_prompted: Some("0.4.0".into()),
            ..Default::default()
        }
        .save();
        assert_eq!(UpdateState::load().last_prompted.as_deref(), Some("0.4.0"));

        // A staged package has to survive the restart it exists for. Its
        // whole point is being applied by a *later* process than the one that
        // downloaded it.
        UpdateState {
            pending: Some(PendingUpdate {
                version: "27.0.0".into(),
                apply_on_launch: true,
                plan_version: PLAN_VERSION,
                updater: PathBuf::from("/tmp/tty7-updater"),
                command: "install".into(),
                rest: vec![PathBuf::from("/tmp/stage/tty7.zip")],
                config_dir: None,
                stage: PathBuf::from("/tmp/stage"),
                needs_elevation: false,
                expected_sha256: None,
            }),
            ..Default::default()
        }
        .save();
        let pending = UpdateState::load().pending.expect("pending round-trips");
        assert_eq!(pending.version, "27.0.0");
        assert!(pending.apply_on_launch);
        assert_eq!(pending.command, "install");

        let _ = std::fs::remove_file(&path);
    }

    /// The rule that replaced "prompted once, then silent forever".
    #[test]
    fn later_defers_a_version_instead_of_retiring_it() {
        let asked = UpdateState {
            last_prompted: Some("27.0.0".into()),
            ..Default::default()
        };
        // Already asked, no expiry set: stay quiet.
        assert!(!should_prompt(&asked, "27.0.0"));
        // A different version is a new question however recently we asked.
        assert!(should_prompt(&asked, "27.1.0"));

        let deferred = UpdateState {
            last_prompted: Some("27.0.0".into()),
            remind_after: Some(now_secs() + 3600),
            ..Default::default()
        };
        assert!(!should_prompt(&deferred, "27.0.0"));

        let due = UpdateState {
            last_prompted: Some("27.0.0".into()),
            remind_after: Some(now_secs().saturating_sub(1)),
            ..Default::default()
        };
        assert!(should_prompt(&due, "27.0.0"));
    }

    /// A failure must not leave the version marked as "already asked about",
    /// or one failed install retires it for good.
    #[test]
    fn a_failure_lets_the_version_prompt_again() {
        let mut state = UpdateState {
            last_prompted: Some("27.0.0".into()),
            remind_after: Some(now_secs() + 3600),
            ..Default::default()
        };
        assert!(!should_prompt(&state, "27.0.0"));

        // The bookkeeping half of `record_failure`, which needs an App to run.
        state.last_failure = Some(FailureRecord {
            version: "27.0.0".into(),
            detail: "downloading tty7-27.0.0-macos-arm64.zip: timed out".into(),
        });
        if state.last_prompted.as_deref() == Some("27.0.0") {
            state.last_prompted = None;
            state.remind_after = None;
        }
        assert!(should_prompt(&state, "27.0.0"));
    }

    /// Serializes the tests that touch the real `update.json` under the
    /// pinned per-process config dir (the pattern `update_guard`'s tests
    /// document: one shared state file, parallel tests, one lock).
    static UPDATE_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn updater_tail_args_carry_config_and_outcome_as_arguments() {
        assert_eq!(
            updater_tail_args(
                Some(Path::new(r"C:\cfg")),
                Some(Path::new(r"C:\cfg\update-outcome.json")),
            ),
            [
                std::ffi::OsString::from("--config-dir"),
                std::ffi::OsString::from(r"C:\cfg"),
                std::ffi::OsString::from("--result-file"),
                std::ffi::OsString::from(r"C:\cfg\update-outcome.json"),
            ]
        );
        // Each flag stands alone; a machine with no resolvable config dir
        // simply passes neither, which is the updater's pre-existing default.
        assert!(updater_tail_args(None, None).is_empty());
        assert_eq!(
            updater_tail_args(None, Some(Path::new("/tmp/outcome.json"))),
            [
                std::ffi::OsString::from("--result-file"),
                std::ffi::OsString::from("/tmp/outcome.json"),
            ]
        );
    }

    /// The whole outcome-file lifecycle in one test: the file and
    /// `update.json` are per-process global state, and parallel tests
    /// writing both would race each other.
    #[test]
    fn an_updater_outcome_is_absorbed_exactly_once() {
        use tty7_core::daemon::install::outcome::{UpdateOutcome, write_outcome};

        let _lock = UPDATE_STATE_LOCK.lock().unwrap();
        crate::core::config::pin_test_config_dir();
        let state_path = UpdateState::path().expect("config dir pinned");
        let outcome_path = update_outcome_path().expect("config dir pinned");
        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(&outcome_path);

        // No outcome: nothing changes, nothing is invented.
        absorb_update_outcome_at_launch();
        let state = UpdateState::load();
        assert!(state.last_failure.is_none() && state.last_prompted.is_none());

        // A failure lands in the state and — the #540 half — does not set the
        // version up to prompt again on its own right away: the throttle is
        // `remind_after`, never a retirement.
        write_outcome(
            &outcome_path,
            &UpdateOutcome {
                version: "27.0.0".into(),
                ok: false,
                detail: Some("the installer exited with code 5".into()),
            },
        )
        .unwrap();
        absorb_update_outcome_at_launch();
        let mut state = UpdateState::load();
        assert_eq!(
            state.last_failure,
            Some(FailureRecord {
                version: "27.0.0".into(),
                detail: "the installer exited with code 5".into(),
            })
        );
        assert!(!should_prompt(&state, "27.0.0"));
        // The version is throttled, not retired: once the reminder expires the
        // question comes back — the same invariant
        // `a_failure_lets_the_version_prompt_again` pins for the in-process
        // path.
        let due = state
            .remind_after
            .expect("a failed install defers the next prompt rather than retiring it");
        assert!(due > now_secs());
        state.remind_after = Some(now_secs().saturating_sub(1));
        assert!(should_prompt(&state, "27.0.0"));
        assert!(!outcome_path.exists(), "the outcome is consumed once");

        // A later launch finds no file and changes nothing.
        absorb_update_outcome_at_launch();
        assert_eq!(
            UpdateState::load()
                .last_failure
                .as_ref()
                .map(|f| &f.version),
            Some(&"27.0.0".to_string())
        );

        // A success retires the failure recorded for that same version. The
        // two name the running version here because the crate version is the
        // only "current" a test can have.
        write_outcome(
            &outcome_path,
            &UpdateOutcome {
                version: current_version().to_string(),
                ok: false,
                detail: Some("first attempt failed".into()),
            },
        )
        .unwrap();
        absorb_update_outcome_at_launch();
        assert_eq!(
            UpdateState::load()
                .last_failure
                .as_ref()
                .map(|f| &f.version),
            Some(&current_version().to_string())
        );
        write_outcome(
            &outcome_path,
            &UpdateOutcome {
                version: current_version().to_string(),
                ok: true,
                detail: None,
            },
        )
        .unwrap();
        absorb_update_outcome_at_launch();
        assert_eq!(UpdateState::load().last_failure, None);
        assert!(!outcome_path.exists());

        // A success naming some other version is stale (a hand-installed
        // rollback in between): dropped, never shown.
        write_outcome(
            &outcome_path,
            &UpdateOutcome {
                version: "99.0.0".into(),
                ok: true,
                detail: None,
            },
        )
        .unwrap();
        absorb_update_outcome_at_launch();
        assert_eq!(UpdateState::load().last_failure, None);

        // Garbage is an outcome too: a failure with an odd detail beats
        // silence about an attempt that definitely ran.
        std::fs::write(&outcome_path, b"not json").unwrap();
        absorb_update_outcome_at_launch();
        let state = UpdateState::load();
        assert!(
            state
                .last_failure
                .as_ref()
                .is_some_and(|failure| failure.detail.contains("could not be read")),
            "{:?}",
            state.last_failure
        );
        assert!(!outcome_path.exists());

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn only_our_own_staging_directories_are_swept() {
        assert!(is_stage_name(".tty7-update-abc123"));
        assert!(is_stage_name("tty7-update-abc123"));
        assert!(!is_stage_name("tty7.app"));
        assert!(!is_stage_name(".Trash"));
        assert!(!is_stage_name("tty7-update"));
    }

    #[test]
    fn a_staged_plan_supplies_a_fresh_parent_pid() {
        let prepared = PreparedUpdate {
            updater: PathBuf::from("/tmp/tty7-updater"),
            command: "install".into(),
            rest: vec![PathBuf::from("/tmp/stage/tty7.zip")],
            config_dir: None,
            stage: PathBuf::from("/tmp/stage"),
            needs_elevation: false,
            expected_sha256: None,
        };
        let args = prepared.args();
        assert_eq!(args[0], PathBuf::from("install"));
        // The pid is the one argument that cannot be persisted: a plan written
        // today names a process that is gone by the time it runs.
        assert_eq!(args[1], PathBuf::from(std::process::id().to_string()));
        assert_eq!(args[2], PathBuf::from("/tmp/stage/tty7.zip"));
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

    /// The two hints that are not about one platform's packaging, and so are
    /// the two every build can reach. Both name the thing that is missing —
    /// a hint that only says "the update failed" leaves the reader with
    /// nowhere to go, and both of these have somewhere: the release page, or
    /// a release that is simply not installable and should say so.
    #[test]
    fn the_cross_platform_hints_name_what_is_missing() {
        crate::ui::i18n::set_locale("en");
        let missing = UpdateInstallHint::MissingPackage("tty7-1.0-mystery.tar.gz".to_string());
        let text = localized_update_install_hint(&missing);
        assert!(
            text.contains("tty7-1.0-mystery.tar.gz"),
            "the hint has to name the package it looked for: {text}"
        );
        assert!(text.contains("release page"), "{text}");

        let checksums = localized_update_install_hint(&UpdateInstallHint::MissingChecksums);
        assert!(checksums.contains("checksums.txt"), "{checksums}");
        assert!(
            checksums.contains("refuses"),
            "the hint has to say tty7 declined on purpose, not that it broke: {checksums}"
        );
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
        assert_eq!(
            windows_layout_updatability(&layout),
            WindowsUpdatability::Updatable
        );

        assert_eq!(
            select_release_asset_for(Err(UpdateInstallHint::WindowsAllUsersInstall), &[]).reason,
            Some(UpdateInstallHint::WindowsAllUsersInstall)
        );
        crate::ui::i18n::set_locale("en");
        let hint = localized_update_install_hint(&UpdateInstallHint::WindowsAllUsersInstall);
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
            windows_layout_updatability(&WindowsUpdateLayout::Portable(directory)),
            WindowsUpdatability::Updatable
        );

        let missing = root.path().join("gone");
        assert!(!windows_directory_is_writable(&missing));
        assert_eq!(
            windows_layout_updatability(&WindowsUpdateLayout::Portable(missing)),
            WindowsUpdatability::Unsupported(UpdateInstallHint::UnsupportedWindows)
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

    /// A plan persisted by a build from before the tail-argument protocol
    /// cannot be carried out by its own staged helper: it relied on the
    /// environment, which an elevated child never inherits (#504). The plan
    /// version is what quietly drops those plans at the next launch instead
    /// of failing weirdly against a helper that does not understand its
    /// arguments.
    #[test]
    fn a_plan_from_another_protocol_version_is_not_usable() {
        let root = tempfile::tempdir().unwrap();
        let updater = root.path().join("tty7-updater");
        std::fs::write(&updater, b"test updater").unwrap();
        let stage = root.path().join("stage");
        std::fs::create_dir(&stage).unwrap();

        let plan = |plan_version| PendingUpdate {
            version: "27.0.0".into(),
            apply_on_launch: true,
            plan_version,
            updater: updater.clone(),
            command: "install".into(),
            rest: vec![stage.join("tty7.zip")],
            config_dir: None,
            stage: stage.clone(),
            needs_elevation: false,
            expected_sha256: None,
        };
        assert!(plan(PLAN_VERSION).is_usable());
        // Plans written before the field existed deserialize it as 0.
        assert!(!plan(0).is_usable());
        assert!(!plan(PLAN_VERSION + 1).is_usable());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn an_elevation_capable_updater_is_recognised_by_its_own_answer() {
        assert!(capabilities_cover_elevation(
            b"elevated-stage\ninstall-elevated\nrelaunch-watcher\n"
        ));
        // Order and unrelated tokens are the updater's business.
        assert!(capabilities_cover_elevation(
            b"install\nrelaunch-watcher\nelevated-stage\ninstall-elevated\n"
        ));
        // An old updater exits with a usage error: no tokens, no chain, and
        // the install falls back to pointing at the release page.
        assert!(!capabilities_cover_elevation(
            b"usage: tty7-updater <command> [args]"
        ));
        assert!(!capabilities_cover_elevation(b""));
        // Two of the three verbs is not the chain.
        assert!(!capabilities_cover_elevation(
            b"elevated-stage\ninstall-elevated\n"
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn an_elevated_plan_names_every_piece_the_chain_needs() {
        let digest = "ab".repeat(32);
        let stage = PathBuf::from(r"C:\Users\someone\AppData\Local\Temp\tty7-update-x");
        let plan = |needs_elevation, expected_sha256: Option<&str>| PendingUpdate {
            version: "27.0.0".into(),
            apply_on_launch: false,
            plan_version: PLAN_VERSION,
            updater: stage.join("tty7-updater.exe"),
            command: "install".into(),
            // The order `prepare_windows_update` pushes: installer, checksums,
            // asset name, install dir, version, log, stage.
            rest: vec![
                stage.join("tty7-27.0.0-windows-x86_64-setup.exe"),
                stage.join("checksums.txt"),
                PathBuf::from("tty7-27.0.0-windows-x86_64-setup.exe"),
                PathBuf::from(r"C:\Program Files\tty7"),
                PathBuf::from("27.0.0"),
                stage.join("update.log"),
                stage.clone(),
            ],
            config_dir: Some(PathBuf::from(r"C:\Users\someone\.config\tty7")),
            stage: stage.clone(),
            needs_elevation,
            expected_sha256: expected_sha256.map(str::to_string),
        };

        let parts = plan(true, Some(&digest))
            .elevated_parts()
            .expect("a complete elevated plan yields its parts");
        assert_eq!(
            parts.installer,
            stage.join("tty7-27.0.0-windows-x86_64-setup.exe")
        );
        assert_eq!(parts.asset_name, "tty7-27.0.0-windows-x86_64-setup.exe");
        assert_eq!(parts.install_dir, PathBuf::from(r"C:\Program Files\tty7"));
        assert_eq!(parts.version, "27.0.0");
        assert_eq!(parts.expected_sha256, digest);

        // Not flagged, or flagged but missing the digest that anchors the
        // chain's trust, is not an elevated plan at all.
        assert!(plan(false, Some(&digest)).elevated_parts().is_none());
        assert!(plan(true, None).elevated_parts().is_none());
        let mut short = plan(true, Some(&digest));
        short.rest.truncate(6);
        assert!(short.elevated_parts().is_none());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn the_elevated_command_line_quotes_every_argument() {
        let parts = ElevatedPlanParts {
            installer: PathBuf::from(r"C:\Users\some one\stage\setup.exe"),
            asset_name: "tty7-27.0.0-windows-x86_64-setup.exe".into(),
            install_dir: PathBuf::from(r"C:\Program Files\tty7"),
            version: "27.0.0".into(),
            log: PathBuf::from(r"C:\Users\some one\stage\update.log"),
            stage: PathBuf::from(r"C:\Users\some one\stage"),
            expected_sha256: "ab".repeat(32),
        };
        let line = elevated_stage_arguments(
            &parts,
            Path::new(r"C:\Users\some one\.config\tty7\update-elevation.status"),
            Path::new(r"C:\Users\some one\.config\tty7\update-outcome.json"),
            Some(Path::new(r"C:\Users\some one\.config\tty7")),
        )
        .into_string()
        .expect("the test paths are valid Unicode");

        assert!(line.starts_with("\"elevated-stage\" "), "{line}");
        assert!(
            line.contains(&format!("\"{}\"", std::process::id())),
            "{line}"
        );
        // A space in a path must never reach the CRT splitter unquoted.
        assert!(line.contains(r#""C:\Program Files\tty7""#), "{line}");
        assert!(
            line.contains(r#""C:\Users\some one\stage\setup.exe""#),
            "{line}"
        );
        assert!(
            line.contains(&format!(
                "\"--expected-sha256\" \"{}\"",
                parts.expected_sha256
            )),
            "{line}"
        );
        assert!(
            line.contains(
                r#""--status-file" "C:\Users\some one\.config\tty7\update-elevation.status""#
            ),
            "{line}"
        );
        assert!(
            line.contains(r#""--config-dir" "C:\Users\some one\.config\tty7""#),
            "{line}"
        );
        assert!(
            line.contains(
                r#""--result-file" "C:\Users\some one\.config\tty7\update-outcome.json""#
            ),
            "{line}"
        );
    }

    /// The CRT splitter the elevated helper is parsed by treats a backslash
    /// as an escape only in front of a quote — so a config directory ending
    /// in one would otherwise escape its own closing quote and eat every
    /// argument after it, including `--result-file`, which is the file the
    /// watcher waits on.
    #[cfg(target_os = "windows")]
    #[test]
    fn a_trailing_backslash_does_not_escape_its_own_closing_quote() {
        let quote = |value: &str| {
            let mut line = std::ffi::OsString::new();
            push_quoted(&mut line, std::ffi::OsStr::new(value));
            line.into_string().expect("valid Unicode in, valid out")
        };

        assert_eq!(
            quote(r"C:\Program Files\tty7"),
            r#""C:\Program Files\tty7""#
        );
        // Doubled only in front of the closing quote.
        assert_eq!(quote(r"D:\tty7\"), r#""D:\tty7\\""#);
        assert_eq!(quote(r"D:\tty7\\"), r#""D:\tty7\\\\""#);
        // An embedded quote is escaped, and the run in front of it doubled.
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(r#"a\"b"#), r#""a\\\"b""#);
        // Interior backslashes are literal: doubling them would rename paths.
        assert_eq!(quote(r"a\b"), r#""a\b""#);

        let mut line = std::ffi::OsString::new();
        push_quoted(&mut line, std::ffi::OsStr::new("one"));
        push_quoted(&mut line, std::ffi::OsStr::new("two"));
        assert_eq!(line.into_string().unwrap(), r#""one" "two""#);
    }
}

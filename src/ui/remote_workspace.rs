use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{Context, PromptLevel, Window};
use gpui_component::WindowExt as _;
use tty7_core::host::HostId;
use tty7_core::host::remote::RemoteHost;

use crate::core::session::{RemoteRef, RemoteTarget, WorkspaceId, WorkspaceStore};
use crate::daemon::control::{ControlEvent, ControlRequest, ReplyOk};
use crate::daemon::install::InstallDecision;
use crate::ui::app::Tty7App;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use crate::ui::remote_connect::{self, HostChoice, RemoteWorkspaceRow};

pub enum ConnectFlow {
    Connecting { choice: HostChoice },
    Failed { choice: HostChoice, error: String },
}

impl ConnectFlow {
    pub fn choice(&self) -> Option<&HostChoice> {
        match self {
            ConnectFlow::Connecting { choice } | ConnectFlow::Failed { choice, .. } => Some(choice),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteStatus {
    Disconnected,
    Connecting,
    Attached,
    Reconnecting {
        attempt: u32,
        /// Why the previous attempt did not land. The pump overwrites the
        /// state back to `Reconnecting` a quarter of a second after an attempt
        /// fails, so without carrying the reason along the strip counts
        /// attempts at a user who was never told what is going wrong.
        last_error: Option<String>,
    },
    Preempted {
        by: String,
    },
    Failed(String),
    /// The route itself is gone — the profile was deleted or the alias left
    /// the ssh config — so no reconnect can ever succeed (#485). Parked: no
    /// retry button, just the truth plus the way back.
    RouteLost,
}

impl RemoteStatus {
    pub fn strip_message(&self, machine: &str) -> Option<String> {
        match self {
            RemoteStatus::Attached => None,
            RemoteStatus::Disconnected => Some(t_fmt(
                L10nKey::RemoteStripDisconnected,
                &[("machine", machine)],
            )),
            RemoteStatus::Connecting => Some(t_fmt(
                L10nKey::RemoteStripConnecting,
                &[("machine", machine)],
            )),
            RemoteStatus::Reconnecting {
                attempt,
                last_error,
            } => Some(match (*attempt, last_error.as_deref()) {
                (0, None) => t_fmt(L10nKey::RemoteStripReconnecting, &[("machine", machine)]),
                (0, Some(error)) => t_fmt(
                    L10nKey::RemoteStripReconnectingWhy,
                    &[("machine", machine), ("error", error)],
                ),
                (attempt, None) => t_fmt(
                    L10nKey::RemoteStripReconnectingAttempt,
                    &[("machine", machine), ("count", &(attempt + 1).to_string())],
                ),
                (attempt, Some(error)) => t_fmt(
                    L10nKey::RemoteStripReconnectingAttemptWhy,
                    &[
                        ("machine", machine),
                        ("count", &(attempt + 1).to_string()),
                        ("error", error),
                    ],
                ),
            }),
            RemoteStatus::Preempted { by } => {
                Some(t_fmt(L10nKey::RemoteStripPreempted, &[("by", by)]))
            }
            RemoteStatus::Failed(e) => Some(t_fmt(
                L10nKey::RemoteStripFailed,
                &[("machine", machine), ("error", e)],
            )),
            RemoteStatus::RouteLost => Some(t_fmt(
                L10nKey::RemoteStripRouteLost,
                &[("machine", machine)],
            )),
        }
    }

    pub fn input_notice(&self) -> Option<&'static str> {
        match self {
            RemoteStatus::Attached => None,
            RemoteStatus::Preempted { .. } => Some(t(L10nKey::RemoteNoticePreempted)),
            _ => Some(t(L10nKey::RemoteNoticeDisconnected)),
        }
    }

    pub fn action_label(&self) -> Option<&'static str> {
        match self {
            RemoteStatus::Attached | RemoteStatus::Connecting => None,
            RemoteStatus::Reconnecting { .. } => Some(t(L10nKey::RemoteActionRetryNow)),
            RemoteStatus::Preempted { .. } => Some(t(L10nKey::RemoteActionTakeBack)),
            RemoteStatus::Disconnected => Some(t(L10nKey::RemoteActionConnect)),
            RemoteStatus::Failed(_) => Some(t(L10nKey::RemoteActionRetry)),
            // A retry on a dead route fails deterministically; the switcher
            // row carries the honest actions (forget the entry, or dismiss).
            RemoteStatus::RouteLost => None,
        }
    }

    #[allow(
        dead_code,
        reason = "reached through `workspace_accepts_input`, whose callers are in terminal/view.rs"
    )]
    pub fn accepts_input(&self) -> bool {
        matches!(self, RemoteStatus::Attached)
    }
}

pub const RECONNECT_FIRST: std::time::Duration = std::time::Duration::from_secs(1);
pub const RECONNECT_CAP: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn delay(&self) -> std::time::Duration {
        let secs = 1u64.checked_shl(self.attempt).unwrap_or(u64::MAX);
        RECONNECT_FIRST
            .saturating_mul(u32::try_from(secs).unwrap_or(u32::MAX))
            .min(RECONNECT_CAP)
    }

    pub fn advance(&mut self) -> std::time::Duration {
        let delay = self.delay();
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[derive(Default)]
#[allow(
    dead_code,
    reason = "the prompt relay that calls this is the other half of D7's start-up connect"
)]
pub struct AuthSheetQueue {
    holder: Option<HostId>,
    waiting: std::collections::VecDeque<HostId>,
}

#[allow(
    dead_code,
    reason = "the prompt relay that calls this is the other half of D7's start-up connect"
)]
impl AuthSheetQueue {
    pub fn request(&mut self, who: HostId) -> bool {
        match self.holder {
            Some(current) if current == who => true,
            Some(_) => {
                if !self.waiting.contains(&who) {
                    self.waiting.push_back(who);
                }
                false
            }
            None => {
                self.holder = Some(who);
                self.waiting.retain(|w| *w != who);
                true
            }
        }
    }

    pub fn release(&mut self, who: HostId) -> Option<HostId> {
        if self.holder != Some(who) {
            self.waiting.retain(|w| *w != who);
            return None;
        }
        self.holder = self.waiting.pop_front();
        self.holder
    }

    pub fn withdraw(&mut self, who: HostId) {
        self.waiting.retain(|w| *w != who);
    }

    pub fn holder(&self) -> Option<HostId> {
        self.holder
    }

    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }
}

impl Tty7App {
    pub(crate) fn spawn_host(&self, cx: &gpui::App) -> HostId {
        WorkspaceStore::host_of(cx, self.workspace)
    }

    pub(crate) fn can_spawn_locally(&self, cx: &gpui::App) -> bool {
        self.spawn_host(cx).is_local()
    }

    pub(crate) fn guard_local_spawn(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.can_spawn_locally(cx) {
            return true;
        }
        match self.window_workspace(cx) {
            Some(ws) if ws.route_header().is_ok() => true,
            _ => {
                let machine = self.remote_machine_label(cx);
                window.push_notification(
                    t_fmt(L10nKey::RemoteNoConnectionDetails, &[("machine", &machine)]),
                    cx,
                );
                false
            }
        }
    }

    pub(crate) fn window_workspace(
        &self,
        cx: &gpui::App,
    ) -> Option<crate::terminal::PaneWorkspace> {
        pane_workspace_for(cx, self.workspace)
    }

    pub(crate) fn remote_machine_label(&self, cx: &gpui::App) -> String {
        match WorkspaceStore::remote_ref(cx, self.workspace) {
            Some(host) => remote_connect::route_label(cx, &host),
            None => t(L10nKey::RemoteThisComputer).to_string(),
        }
    }

    pub(crate) fn rebind_host(&mut self, previous: HostId, cx: &gpui::App) {
        if crate::core::session::crosses_machines(previous, self.spawn_host(cx)) {
            self.closed.clear();
        }
    }

    pub(crate) fn default_shell_label(&self, _cx: &gpui::App) -> String {
        // ShellInventory maps configured programs to their displayed labels,
        // including friendly Windows names that differ from the executable.
        self.shells.default_name.clone()
    }

    pub(crate) fn refresh_shells(&mut self, cx: &mut Context<Self>) {
        let host_id = self.spawn_host(cx);
        self.shells_host = host_id;
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, host_id) else {
            self.shells = Default::default();
            cx.notify();
            return;
        };
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            |h| h.shells(),
            move |app, out, cx| {
                if app.shells_host != host_id {
                    return;
                }
                app.shells = match out {
                    Ok(inventory) => inventory,
                    Err(e) => {
                        log::warn!("could not list the shells of this window's machine: {e}");
                        Default::default()
                    }
                };
                cx.notify();
            },
        );
    }

    pub(crate) fn panes(&self) -> Vec<gpui::Entity<crate::terminal::view::TerminalView>> {
        self.tabs
            .iter()
            .flat_map(|tab| tab.pane.terminals())
            .collect()
    }

    pub(crate) fn remote_status(&self, cx: &gpui::App) -> Option<RemoteStatus> {
        let own = WorkspaceStore::remote_ref(cx, self.workspace)?;
        let supervised = RemoteLinks::status_of(cx, self.workspace);
        let resolvable = remote_connect::route_resolvable(cx, &own.target);
        resolve_status(self.connect.as_ref(), &own.target, supervised, resolvable)
    }

    pub(crate) fn remote_retry(&mut self, cx: &mut Context<Self>) {
        match &self.connect {
            Some(ConnectFlow::Failed { choice, .. }) => {
                let choice = choice.clone();
                self.connect_to_host(choice, cx);
            }
            _ => RemoteLinks::retry_now(cx, self.workspace),
        }
        cx.notify();
    }

    pub(crate) fn connect_to_host(&mut self, choice: HostChoice, cx: &mut Context<Self>) {
        remote_connect::register(cx);
        // Whatever went wrong last time is about to be answered by this attempt.
        self.remote_host_errors.remove(&choice.target.to_string());
        let header = match remote_connect::control_route(&choice.target, cx) {
            Ok(header) => header,
            Err(e) => {
                self.connect = Some(ConnectFlow::Failed { choice, error: e });
                cx.notify();
                return;
            }
        };
        let target = choice.target.clone();
        let label = choice.label.clone();
        cx.default_global::<RemoteLinks>()
            .suspended
            .remove(&choice.target.host_id());
        self.connect = Some(ConnectFlow::Connecting {
            choice: choice.clone(),
        });
        cx.notify();

        remote_connect::clear_install_progress(choice.target.host_id());
        self.watch_for_install_consent(choice.target.host_id(), cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { remote_connect::connect_blocking(&target, header, &label) })
                .await;
            let _ = this.update(cx, |this, cx| this.finish_connect(result, cx));
        })
        .detach();
    }

    fn watch_for_install_consent(&self, host: HostId, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut painted: Option<crate::daemon::install::InstallPhase> = None;
            loop {
                let connecting = this
                    .update(cx, |this, _| {
                        matches!(this.connect, Some(ConnectFlow::Connecting { .. }))
                    })
                    .unwrap_or(false);
                if !connecting {
                    return;
                }
                if let Some(pending) = remote_connect::take_pending_install() {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.prompt_install_consent(pending, window, cx)
                    });
                }
                let reported = remote_connect::install_progress_for(host);
                if reported != painted {
                    painted = reported;
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
                cx.update(pump_auth_sheets);
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
            }
        })
        .detach();
    }

    fn finish_connect(
        &mut self,
        result: Result<remote_connect::Connected, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(choice) = self.connect.as_ref().and_then(ConnectFlow::choice).cloned() else {
            return;
        };
        remote_connect::clear_install_progress(choice.target.host_id());
        match result {
            Ok(connected) => {
                let home = connected.home.clone();
                let rows = connected.rows.clone();
                self.host_snapshots.insert(
                    choice.target.host_id(),
                    crate::ui::switcher::HostSnapshot {
                        target: choice.target.clone(),
                        rows: rows.clone(),
                    },
                );
                remote_connect::HostLinks::insert(cx, connected.host, home.clone());
                self.prompt_remote_daemon_mismatch_later(cx);
                self.connect = None;
            }
            Err(error) => {
                log::warn!("connect to {} failed: {error}", choice.label);
                self.connect = Some(ConnectFlow::Failed { choice, error });
            }
        }
        cx.notify();
    }

    pub(crate) fn open_remote_workspace(
        &mut self,
        target: RemoteTarget,
        row: RemoteWorkspaceRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = RemoteRef::new(target, row.id);
        let id = WorkspaceStore::claim_remote(cx, host);
        self.enter_remote_workspace(id, window, cx);
    }

    pub(crate) fn create_remote_workspace(
        &mut self,
        target: RemoteTarget,
        home: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = RemoteRef::new(target.clone(), WorkspaceId::new());
        let id = WorkspaceStore::claim_remote(cx, host);
        log::info!(
            "new remote workspace on {target} rooted at {}",
            home.display()
        );
        self.enter_remote_workspace(id, window, cx);
    }

    fn enter_remote_workspace(
        &mut self,
        id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connect = None;
        RemoteLinks::ensure_running(cx);
        let previous = self.spawn_host(cx);
        self.switch_workspace(Some(id), window, cx);
        self.rebind_host(previous, cx);
        cx.notify();
    }

    pub(crate) fn reopen_remote_at_startup(&self, cx: &mut Context<Self>) {
        RemoteLinks::supervise(cx, self.workspace);
    }

    pub(crate) fn prompt_install_consent(
        &mut self,
        pending: remote_connect::PendingInstall,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = remote_connect::install_title(&pending.request);
        let detail = remote_connect::install_detail(&pending.request);
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(&detail),
            &crate::ui::confirm_answers(t(L10nKey::SettingsInstall), t(L10nKey::Cancel)),
            cx,
        );
        cx.spawn(async move |_, _| {
            let decision = match answer.await {
                Ok(0) => InstallDecision::Approve,
                _ => InstallDecision::Decline,
            };
            pending.answer(decision);
        })
        .detach();
    }

    pub(crate) fn prompt_remote_daemon_mismatch(window: &mut Window, cx: &mut Context<Self>) {
        let queue = crate::daemon::install::take_mismatched_remote_daemons();
        Self::ask_next_daemon_mismatch(queue, window, cx);
    }

    /// One question at a time. Reconnecting to three machines whose servers are
    /// all behind used to raise three native modals in a single pass, stacked
    /// on each other, each about a machine the previous one did not name.
    fn ask_next_daemon_mismatch(
        mut queue: Vec<crate::daemon::install::MismatchedRemoteDaemon>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mismatch) = queue.pop() else {
            return;
        };
        let title = remote_connect::mismatch_title(&mismatch);
        let detail = remote_connect::mismatch_detail(&mismatch);
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(&detail),
            &remote_connect::mismatch_answers(),
            cx,
        );
        cx.spawn(async move |this, cx| {
            let answered = answer.await;
            let restart = matches!(answered, Ok(0));
            // The window went away with the question open. Arm the rest again
            // rather than swallowing machines nobody was ever asked about.
            if answered.is_err() {
                queue.push(mismatch.clone());
                crate::daemon::install::record_remote_mismatches(queue);
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                if restart {
                    this.restart_mismatched_remote_server(mismatch, window, cx);
                }
                Self::ask_next_daemon_mismatch(queue, window, cx);
            });
        })
        .detach();
    }

    fn restart_mismatched_remote_server(
        &mut self,
        mismatch: crate::daemon::install::MismatchedRemoteDaemon,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = mismatch.host.clone();
        match remote_connect::mismatch_target(&mismatch) {
            Some(target) => self.replace_remote_server(target, label, window, cx),
            None => {
                let e = t_fmt(L10nKey::RemoteNoRouteToHost, &[("machine", &label)]);
                self.report_remote_host_error(None, &label, &e, window, cx);
            }
        }
    }

    pub(crate) fn confirm_restart_remote_server(
        &mut self,
        target: RemoteTarget,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &t_fmt(L10nKey::RemoteRestartTitle, &[("machine", &label)]),
            Some(&t_fmt(L10nKey::RemoteRestartBody, &[("machine", &label)])),
            &crate::ui::confirm_answers(t(L10nKey::RestartServer), t(L10nKey::Cancel)),
            cx,
        );
        cx.spawn(async move |this, cx| {
            if !matches!(answer.await, Ok(0)) {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.restart_remote_server(target, label, window, cx);
            });
        })
        .detach();
    }

    fn restart_remote_server(
        &mut self,
        target: RemoteTarget,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_remote_host_error(&target);
        let header = match remote_connect::control_route(&target, cx) {
            Ok(header) => header.restart_server(),
            Err(e) => {
                self.report_remote_host_error(Some(&target), &label, &e, window, cx);
                return;
            }
        };
        let host = header.target.origin_key();
        let host_id = target.host_id();
        let target_for_error = target.clone();
        log::info!("restarting tty7's server on {label} at the user's request");
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        self.watch_for_restart_consent(host_id, running.clone(), cx);
        cx.spawn(async move |this, cx| {
            let for_task = label.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move { remote_connect::restart_server_blocking(header, &for_task) })
                .await;
            running.store(false, std::sync::atomic::Ordering::Relaxed);
            remote_connect::clear_install_progress(host_id);
            let _ = this.update_in(cx, |this, window, cx| match outcome {
                Ok(()) => {
                    log::info!("{label} is now serving this client's build");
                    reconnect_after_restart(&host, cx);
                }
                Err(e) => {
                    log::warn!("could not restart tty7's server on {label}: {e}");
                    this.report_remote_host_error(Some(&target_for_error), &label, &e, window, cx);
                }
            });
        })
        .detach();
    }

    pub(crate) fn confirm_replace_remote_server(
        &mut self,
        target: RemoteTarget,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &t_fmt(L10nKey::RemoteMismatchTitle, &[("machine", &label)]),
            Some(&t_fmt(L10nKey::RemoteReplaceBody, &[("machine", &label)])),
            &crate::ui::confirm_answers(
                t(L10nKey::RemoteMismatchReplaceServer),
                t(L10nKey::Cancel),
            ),
            cx,
        );
        cx.spawn(async move |this, cx| {
            if !matches!(answer.await, Ok(0)) {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                this.replace_remote_server(target, label, window, cx);
            });
        })
        .detach();
    }

    fn replace_remote_server(
        &mut self,
        target: RemoteTarget,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_remote_host_error(&target);
        let route = match remote_connect::control_route(&target, cx) {
            Ok(header) => header.replace_server(),
            Err(e) => {
                log::warn!("could not address {label} to replace its server: {e}");
                self.report_remote_host_error(Some(&target), &label, &e, window, cx);
                return;
            }
        };
        let host = route.target.origin_key();
        let host_id = target.host_id();
        let target_for_error = target.clone();
        log::info!("replacing tty7's server on {label} at the user's request");
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        self.watch_for_restart_consent(host_id, running.clone(), cx);
        cx.spawn(async move |this, cx| {
            let for_task = label.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move { remote_connect::restart_server_blocking(route, &for_task) })
                .await;
            running.store(false, std::sync::atomic::Ordering::Relaxed);
            remote_connect::clear_install_progress(host_id);
            let _ = this.update_in(cx, |this, window, cx| match outcome {
                Ok(()) => {
                    log::info!("{label} is now serving this client's build");
                    reconnect_after_restart(&host, cx);
                }
                Err(e) => {
                    log::warn!("could not replace tty7's server on {label}: {e}");
                    this.report_remote_host_error(Some(&target_for_error), &label, &e, window, cx);
                }
            });
        })
        .detach();
    }

    /// Retire everything still on screen about this host's last failure. The
    /// switcher paints a failed `connect` in preference to `remote_host_errors`,
    /// so leaving one behind would show the stale complaint again in place of
    /// whatever this attempt has to say.
    fn clear_remote_host_error(&mut self, target: &RemoteTarget) {
        self.remote_host_errors.remove(&target.to_string());
        if let Some(ConnectFlow::Failed { choice, .. }) = &self.connect
            && &choice.target == target
        {
            self.connect = None;
        }
    }

    fn report_remote_host_error(
        &mut self,
        target: Option<&RemoteTarget>,
        label: &str,
        error: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The grouped report is only visible while the switcher is open. Anywhere
        // else — the window menu's "restart server", or a mismatch raised mid-connect
        // — the modal is the only thing the user would see, so keep it.
        if let (Some(target), Some(switcher)) = (target, self.switcher.as_mut()) {
            let key = target.to_string();
            switcher.expand(&key);
            self.remote_host_errors.insert(key, error.to_string());
            cx.notify();
            return;
        }

        let answer = window.prompt(
            PromptLevel::Warning,
            &t_fmt(L10nKey::RemoteRestartFailedTitle, &[("machine", label)]),
            Some(&t_fmt(
                L10nKey::RemoteRestartFailedBody,
                &[("error", error)],
            )),
            &[t(L10nKey::Ok)],
            cx,
        );
        cx.spawn(async move |_, _| {
            let _ = answer.await;
        })
        .detach();
    }

    fn watch_for_restart_consent(
        &self,
        host: HostId,
        running: Arc<std::sync::atomic::AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let mut painted: Option<crate::daemon::install::InstallPhase> = None;
            while running.load(std::sync::atomic::Ordering::Relaxed) {
                let reported = remote_connect::install_progress_for(host);
                if reported != painted {
                    painted = reported;
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
                cx.update(pump_auth_sheets);
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
        })
        .detach();
    }

    fn prompt_remote_daemon_mismatch_later(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let _ = this.update_in(cx, |_, window, cx| {
                Tty7App::prompt_remote_daemon_mismatch(window, cx);
            });
        })
        .detach();
    }
}

/// Which of the two accounts of this window's machine the strip should believe.
///
/// `connect` is one window's memory of one manual attempt; the supervisor holds
/// every link and keeps holding it long after that attempt is over. Two things
/// follow. A flow that named a *different* machine has nothing to say here — a
/// failed connect to the GPU box used to replace the strip of a window sitting
/// happily on the build box. And once the supervisor reports the link as
/// Attached, or the workspace as taken over, that is what happened, however
/// badly the window's own last attempt went.
///
/// Everything else keeps the old order, so a connect that is still in flight
/// still says so while the supervisor is still calling the machine unknown.
fn resolve_status(
    connect: Option<&ConnectFlow>,
    own: &RemoteTarget,
    supervised: Option<RemoteStatus>,
    route_resolvable: bool,
) -> Option<RemoteStatus> {
    let mine = connect.filter(|flow| flow.choice().is_some_and(|c| &c.target == own));
    if matches!(
        supervised,
        Some(RemoteStatus::Attached | RemoteStatus::Preempted { .. })
    ) {
        return supervised;
    }
    // A route that no longer resolves parks the workspace (#485): nothing it
    // could try would ever succeed, so say that instead of retrying. A live
    // or preempted link outranks it — those panes work regardless of what
    // happened to the profile that made them.
    if !route_resolvable {
        return Some(RemoteStatus::RouteLost);
    }
    match mine {
        Some(ConnectFlow::Connecting { .. }) => Some(RemoteStatus::Connecting),
        Some(ConnectFlow::Failed { error, .. }) => Some(RemoteStatus::Failed(error.clone())),
        None => supervised,
    }
}

pub(crate) fn pane_workspace_for(
    cx: &gpui::App,
    workspace: WorkspaceId,
) -> Option<crate::terminal::PaneWorkspace> {
    let host = WorkspaceStore::remote_ref(cx, workspace)?;
    let spec = remote_connect::spec_for(&host.target, cx)
        .ok()
        .map(|spec| Box::new(spec.without_secrets()));
    let pane = crate::terminal::PaneWorkspace {
        workspace,
        target: host.target,
        spec,
    };
    if let Ok(header) = pane.route_header() {
        remote_connect::note_origin(&header.target, &pane.target);
    }
    Some(pane)
}

pub(crate) fn pane_route_for(cx: &gpui::App, workspace: WorkspaceId) -> crate::terminal::PaneRoute {
    crate::terminal::PaneRoute::for_workspace(pane_workspace_for(cx, workspace).as_ref())
}

pub(crate) const PUMP_TICK: Duration = Duration::from_millis(250);

struct MachineLink {
    state: LinkState,
    backoff: Backoff,
    next_attempt: Option<Instant>,
    attempting: bool,
    /// What the last attempt said when it failed. `Reconnecting` overwrites
    /// `Failed` on the very next tick, so this is the only place the reason
    /// survives long enough for anyone to read it.
    last_error: Option<String>,
    /// The workspaces this client has sent a `WorkspaceAttach` for over the
    /// link that is up right now, whether or not the far end took it. Scoped
    /// to one link on purpose: a new link has heard nothing from us.
    attach_sent: std::collections::HashSet<WorkspaceId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LinkState {
    Connecting,
    Attached,
    Reconnecting,
    Failed(String),
}

/// What the supervisor knows about one *machine's* link, for readers outside
/// this module. `RemoteStatus` answers for a single workspace; the switcher
/// draws a machine, and until it had this it had to guess from whether a
/// `HostLinks` entry happened to exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MachineStatus {
    Connecting,
    Attached,
    Reconnecting {
        attempt: u32,
        last_error: Option<String>,
    },
    Failed(String),
}

#[derive(Default)]
pub(crate) struct RemoteLinks {
    machines: std::collections::HashMap<HostId, MachineLink>,
    preempted: std::collections::HashMap<WorkspaceId, String>,
    /// Workspaces whose takeover the user has asked to undo, each still
    /// carrying the name of the client that displaced it — an attach that
    /// fails has to be able to put the marker back where it found it.
    reclaiming: std::collections::HashMap<WorkspaceId, String>,
    /// Workspaces with a `WorkspaceAttach` on the wire. The pump ticks four
    /// times a second and the request's deadline is ten seconds, so without
    /// this one reclaim would be sent forty times.
    attaching: std::collections::HashSet<WorkspaceId>,
    suspended: std::collections::HashSet<HostId>,
    instances: std::collections::HashMap<HostId, String>,
    #[allow(
        dead_code,
        reason = "read by the prompt relay's drain, which lands with the routed auth sheet"
    )]
    pub(crate) auth: AuthSheetQueue,
    pumping: bool,
}

impl gpui::Global for RemoteLinks {}

static EVENTS: Mutex<Vec<(HostId, ControlEvent)>> = Mutex::new(Vec::new());

pub(crate) fn install_event_observer() {
    crate::daemon::control::set_event_observer(Arc::new(|host, event| {
        if let Ok(mut queue) = EVENTS.lock() {
            queue.push((host, event));
        }
    }));
}

impl RemoteLinks {
    pub(crate) fn ensure_running(cx: &mut gpui::App) {
        install_event_observer();
        if cx.default_global::<RemoteLinks>().pumping {
            return;
        }
        cx.default_global::<RemoteLinks>().pumping = true;
        cx.spawn(async move |cx| {
            loop {
                let carry_on = cx.update(pump_tick);
                if !carry_on {
                    cx.update(|cx| cx.default_global::<RemoteLinks>().pumping = false);
                    return;
                }
                cx.background_executor().timer(PUMP_TICK).await;
            }
        })
        .detach();
    }

    pub(crate) fn supervise(cx: &mut gpui::App, workspace: WorkspaceId) {
        let Some(host) = WorkspaceStore::remote_ref(cx, workspace) else {
            return;
        };
        remote_connect::register(cx);
        log::info!(
            "supervising {} for a workspace that just opened",
            host.target
        );
        RemoteLinks::ensure_running(cx);
    }

    pub(crate) fn status_of(cx: &gpui::App, workspace: WorkspaceId) -> Option<RemoteStatus> {
        let host = WorkspaceStore::remote_ref(cx, workspace)?;
        let Some(links) = cx.try_global::<RemoteLinks>() else {
            return Some(RemoteStatus::Disconnected);
        };
        // A Take Back that is still on the wire is neither of the two things it
        // sits between. It is not Preempted any more — the request to undo that
        // is out — and it is emphatically not Attached: the panes are still
        // released and the far end has not answered yet. Connecting is the
        // honest word, and it is also the one state `action_label` offers no
        // button for, so the same reclaim cannot be started twice.
        if links.reclaiming.contains_key(&workspace) {
            return Some(RemoteStatus::Connecting);
        }
        if let Some(by) = links.preempted.get(&workspace) {
            return Some(RemoteStatus::Preempted { by: by.clone() });
        }
        Some(match links.machines.get(&host.host_id()) {
            Some(link) => match &link.state {
                LinkState::Connecting => RemoteStatus::Connecting,
                LinkState::Attached => RemoteStatus::Attached,
                LinkState::Reconnecting => RemoteStatus::Reconnecting {
                    attempt: link.backoff.attempt(),
                    last_error: link.last_error.clone(),
                },
                LinkState::Failed(e) => RemoteStatus::Failed(e.clone()),
            },
            None => RemoteStatus::Disconnected,
        })
    }

    /// The supervisor's view of one machine, for the switcher. `None` means it
    /// has never had anything to do with this machine — not that the link is
    /// down, which is `Reconnecting`.
    pub(crate) fn machine_status(cx: &gpui::App, host: HostId) -> Option<MachineStatus> {
        let link = cx.try_global::<RemoteLinks>()?.machines.get(&host)?;
        Some(match &link.state {
            LinkState::Connecting => MachineStatus::Connecting,
            LinkState::Attached => MachineStatus::Attached,
            LinkState::Reconnecting => MachineStatus::Reconnecting {
                attempt: link.backoff.attempt(),
                last_error: link.last_error.clone(),
            },
            LinkState::Failed(e) => MachineStatus::Failed(e.clone()),
        })
    }

    /// The open workspaces on this machine that another client is holding.
    pub(crate) fn preempted_on(cx: &gpui::App, host: HostId) -> Vec<WorkspaceId> {
        let Some(links) = cx.try_global::<RemoteLinks>() else {
            return Vec::new();
        };
        workspaces_on(cx, host)
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| links.preempted.contains_key(id))
            .collect()
    }

    pub(crate) fn retry_now(cx: &mut gpui::App, workspace: WorkspaceId) {
        let Some(host) = WorkspaceStore::remote_ref(cx, workspace) else {
            return;
        };
        let links = cx.default_global::<RemoteLinks>();
        if let Some(by) = links.preempted.remove(&workspace) {
            links.reclaiming.insert(workspace, by);
        }
        links.suspended.remove(&host.host_id());
        let link = links.machines.entry(host.host_id()).or_insert(MachineLink {
            state: LinkState::Reconnecting,
            backoff: Backoff::default(),
            next_attempt: None,
            attempting: false,
            last_error: None,
            attach_sent: Default::default(),
        });
        link.backoff.reset();
        link.next_attempt = Some(Instant::now());
        if !link.attempting {
            link.state = LinkState::Reconnecting;
        }
        RemoteLinks::ensure_running(cx);
        cx.refresh_windows();
    }

    pub(crate) fn disconnect(cx: &mut gpui::App, host: HostId) {
        cx.default_global::<RemoteLinks>().suspended.insert(host);
        for (workspace, _) in workspaces_on(cx, host) {
            release_panes(cx, workspace);
            let links = cx.default_global::<RemoteLinks>();
            links.preempted.remove(&workspace);
            links.reclaiming.remove(&workspace);
            links.attaching.remove(&workspace);
        }
        remote_connect::HostLinks::remove(cx, host);
        cx.default_global::<RemoteLinks>().machines.remove(&host);
        log::info!("disconnected from a machine at the user's request");
        cx.refresh_windows();
    }

    fn mark(cx: &mut gpui::App, host: HostId, f: impl FnOnce(&mut MachineLink)) {
        let link = cx
            .default_global::<RemoteLinks>()
            .machines
            .entry(host)
            .or_insert(MachineLink {
                state: LinkState::Connecting,
                backoff: Backoff::default(),
                next_attempt: None,
                attempting: false,
                last_error: None,
                attach_sent: Default::default(),
            });
        f(link);
    }
}

fn pump_tick(cx: &mut gpui::App) -> bool {
    drain_events(cx);
    pump_auth_sheets(cx);

    let bound = bound_machines(cx);
    if bound.is_empty() {
        let links = cx.default_global::<RemoteLinks>();
        let forgotten = links.machines.len();
        links.machines.clear();
        links.preempted.clear();
        links.reclaiming.clear();
        links.attaching.clear();
        links.suspended.clear();
        log::info!("supervisor stopped: no open remote workspace ({forgotten} link(s) dropped)");
        return false;
    }

    prune_suspended(&mut cx.default_global::<RemoteLinks>().suspended, &bound);
    let suspended = cx.default_global::<RemoteLinks>().suspended.clone();

    let now = Instant::now();
    let mut changed = false;
    for (host, target) in bound {
        if suspended.contains(&host) {
            continue;
        }
        let live =
            remote_connect::HostLinks::get(cx, host).is_some_and(|h| h.client().is_connected());
        let attempting = cx
            .try_global::<RemoteLinks>()
            .and_then(|l| l.machines.get(&host))
            .is_some_and(|l| l.attempting);

        if live {
            let mut became = false;
            RemoteLinks::mark(cx, host, |link| {
                became = link.state != LinkState::Attached;
                link.state = LinkState::Attached;
                link.backoff.reset();
                link.next_attempt = None;
                link.last_error = None;
            });
            // A live link is not an attachment. Preemption of a GUI client
            // leaves the control connection alone (only a `dedicated` client is
            // hung up on), and a link brought up by the switcher's own connect
            // never sends `WorkspaceAttach` at all — so the far end can be
            // talking to us happily while holding none of our workspaces. This
            // runs before anything below that touches a window's contents: a
            // workspace wants to be claimed for this client before we start
            // rebuilding its tabs and their panes on the machine.
            pump_attachments(cx, host);
            if became {
                changed = true;
                log::info!("link to {target} is attached");
                crate::ui::machine_mirror::MachineMirrors::refresh(cx, host);
                // Some window watched an earlier attempt to this machine fail
                // and is still saying so. The supervisor is the one that got
                // through, so nobody else is going to retire that.
                clear_window_failures_for(cx, &target);
                // A link this machine's windows never asked for — the switcher
                // connected it, or `finish_connect` installed it — comes up
                // without any reconnect attempt finishing, so nothing else
                // tells the windows on it that their machine can be reached
                // now. One of them may be sitting empty owing a pull.
                crate::ui::tree_sync::on_link_up(cx, host);
            }
            continue;
        }
        if attempting {
            continue;
        }

        // A route that no longer resolves — the profile was deleted, or the
        // alias left the ssh config — can only fail, deterministically and
        // forever (#485). Park the entry: drop any dead link state, but no
        // backoff, no attempt, no error. Its label comes from the route
        // snapshot, and the switcher offers to forget it.
        let resolvable = remote_connect::route_resolvable(cx, &target);
        if remote_connect::HostLinks::get(cx, host).is_some() {
            remote_connect::HostLinks::remove(cx, host);
            if resolvable {
                log::info!("lost the control connection to {target}; reconnecting");
            }
        }
        if !resolvable {
            continue;
        }

        let due = {
            let mut due = false;
            RemoteLinks::mark(cx, host, |link| {
                if !matches!(link.state, LinkState::Reconnecting) {
                    changed = true;
                    link.state = LinkState::Reconnecting;
                }
                // Whatever we told the far end went down with the link.
                link.attach_sent.clear();
                match link.next_attempt {
                    None => link.next_attempt = Some(now + link.backoff.advance()),
                    Some(at) if at <= now => {
                        due = true;
                        link.next_attempt = None;
                    }
                    Some(_) => {}
                }
            });
            due
        };
        if due {
            launch_attempt(cx, host, target);
            changed = true;
        }
    }

    if changed {
        cx.refresh_windows();
    }
    true
}

/// The workspaces on this machine the far end has not been told about yet,
/// marked as in flight on the way out so the next tick leaves them alone.
///
/// Two things land here. A Take Back sits in `reclaiming` until an attach
/// answers for it, and a workspace nobody has ever attached over the link that
/// is up now is just as detached as one that was displaced — `connect_blocking`
/// brings a control link up and stops there, so every switcher-initiated
/// connect used to leave the daemon with no attachment at all.
///
/// A workspace someone else is holding is deliberately left out: taking it back
/// is the user's call, not the pump's.
fn reclaims_due(cx: &mut gpui::App, host: HostId) -> Vec<(WorkspaceId, String)> {
    let open = workspaces_on(cx, host);
    let links = cx.default_global::<RemoteLinks>();
    let sent = links
        .machines
        .get(&host)
        .map(|link| link.attach_sent.clone())
        .unwrap_or_default();
    let due: Vec<(WorkspaceId, String)> = open
        .into_iter()
        .filter(|(id, _)| !links.attaching.contains(id))
        .filter(|(id, _)| !links.preempted.contains_key(id))
        .filter(|(id, _)| links.reclaiming.contains_key(id) || !sent.contains(id))
        .collect();
    for (id, _) in &due {
        links.attaching.insert(*id);
    }
    due
}

fn pump_attachments(cx: &mut gpui::App, host: HostId) {
    let Some(link) = remote_connect::HostLinks::get(cx, host) else {
        return;
    };
    for (workspace, key) in reclaims_due(cx, host) {
        // Never on the UI thread: the request's deadline is ten seconds, and a
        // far end that has stopped answering would freeze every window.
        let client = Arc::clone(link.client());
        cx.spawn(async move |cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    client
                        .call(ControlRequest::WorkspaceAttach { id: key })
                        .map_err(|e| e.to_string())
                })
                .await;
            cx.update(|cx| finish_reclaim(cx, host, workspace, outcome));
        })
        .detach();
    }
}

fn finish_reclaim(
    cx: &mut gpui::App,
    host: HostId,
    workspace: WorkspaceId,
    outcome: Result<ReplyOk, String>,
) {
    let reclaimed = {
        let links = cx.default_global::<RemoteLinks>();
        links.attaching.remove(&workspace);
        match &outcome {
            Ok(_) => links.reclaiming.remove(&workspace).is_some(),
            // Put the takeover back on the strip. The user asked to undo it and
            // it did not happen, so the window is still read-only and the Take
            // Back button is still the only thing that can change that.
            Err(_) => {
                if let Some(by) = links.reclaiming.remove(&workspace) {
                    links.preempted.insert(workspace, by);
                }
                false
            }
        }
    };
    let failure = match outcome {
        Ok(ReplyOk::Attached {
            took_over_from: Some(who),
        }) => {
            log::info!("workspace {workspace} taken back from {who}");
            None
        }
        Ok(_) => None,
        Err(e) => {
            log::warn!("could not attach to workspace {workspace}: {e}");
            Some(e)
        }
    };
    RemoteLinks::mark(cx, host, |link| {
        // Sent either way. A refusal that repeats every 250ms would be a flood,
        // and the user has a Take Back button for the one case worth retrying.
        link.attach_sent.insert(workspace);
        if failure.is_some() {
            link.last_error = failure.clone();
        }
    });
    if reclaimed {
        // The other client had this workspace for a while and may have moved
        // every pane in it. Only the tree knows what it looks like now.
        crate::ui::tree_sync::resync_window_from_tree(cx, workspace);
        refresh_window_shells(cx, workspace);
    }
    cx.refresh_windows();
}

/// Retire what the *windows* still say about a failed connect to this machine.
///
/// `ConnectFlow::Failed` and `remote_host_errors` belong to whichever window
/// ran the manual attempt, and the switcher paints both in preference to
/// anything the supervisor knows. A link the supervisor brought up on its own
/// therefore leaves the last failure on screen forever, because the window that
/// recorded it never hears that the machine came back.
fn clear_window_failures_for(cx: &mut gpui::App, target: &RemoteTarget) {
    let host = target.host_id();
    for (_, app) in crate::ui::windows::WindowRegistry::open_windows(cx) {
        let Some(app) = app.upgrade() else {
            continue;
        };
        let key = target.to_string();
        app.update(cx, |app, cx| {
            let failed = match &app.connect {
                Some(ConnectFlow::Failed { choice, .. }) if choice.target.host_id() == host => {
                    Some(choice.target.clone())
                }
                _ => None,
            };
            let had_error = app.remote_host_errors.remove(&key).is_some();
            match failed {
                Some(target) => app.clear_remote_host_error(&target),
                None if !had_error => return,
                None => {}
            }
            cx.notify();
        });
    }
}

fn prune_suspended(
    suspended: &mut std::collections::HashSet<HostId>,
    bound: &[(HostId, RemoteTarget)],
) {
    suspended.retain(|host| bound.iter().any(|(id, _)| id == host));
}

/// Does this machine's entry survive a profile deletion (#485)? A live link
/// or an in-flight attempt does: the connection holds an authenticated spec,
/// not a profile reference, and forgetting the entry would release the link
/// under any window still attached to it.
pub(crate) fn link_alive_or_connecting(cx: &mut gpui::App, host: HostId) -> bool {
    if remote_connect::HostLinks::get(cx, host).is_some() {
        return true;
    }
    cx.try_global::<RemoteLinks>()
        .and_then(|links| links.machines.get(&host))
        .is_some_and(|link| link.attempting || matches!(link.state, LinkState::Connecting))
}

fn bound_machines(cx: &gpui::App) -> Vec<(HostId, RemoteTarget)> {
    let mut out: Vec<(HostId, RemoteTarget)> = Vec::new();
    for workspace in &WorkspaceStore::all(cx).views {
        let Some(host) = workspace.host.as_ref() else {
            continue;
        };
        if !workspace.open {
            continue;
        }
        let id = host.host_id();
        if !out.iter().any(|(seen, _)| *seen == id) {
            out.push((id, host.target.clone()));
        }
    }
    out
}

fn workspaces_on(cx: &gpui::App, host: HostId) -> Vec<(WorkspaceId, String)> {
    WorkspaceStore::all(cx)
        .views
        .iter()
        .filter(|w| w.open)
        .filter_map(|w| {
            let remote = w.host.as_ref()?;
            (remote.host_id() == host).then(|| (w.id, remote.store_key()))
        })
        .collect()
}

pub(crate) fn drain_events(cx: &mut gpui::App) {
    let events = match EVENTS.lock() {
        Ok(mut queue) => std::mem::take(&mut *queue),
        Err(_) => return,
    };
    for (host, event) in events {
        match event {
            ControlEvent::Preempted { workspace, by } => {
                let Some(id) = client_id_for(cx, host, &workspace) else {
                    log::warn!(
                        "preempted on {host:?} for a workspace this client has no window on"
                    );
                    continue;
                };
                log::info!("workspace {id} was taken over by {by}");
                cx.default_global::<RemoteLinks>()
                    .preempted
                    .insert(id, by.clone());
                release_panes(cx, id);
                crate::ui::tree_sync::on_preempted(cx, id);
                cx.refresh_windows();
            }
            ControlEvent::Layout { workspace, delta } => {
                crate::ui::tree_sync::on_layout_delta(cx, host, &workspace, delta);
            }
            ControlEvent::LayoutResync => {
                log::info!("{host:?} dropped layout deltas for this client; re-pulling");
                crate::ui::machine_mirror::MachineMirrors::refresh(cx, host);
                for (workspace, _) in crate::ui::windows::WindowRegistry::open_windows(cx) {
                    if WorkspaceStore::host_of(cx, workspace) != host {
                        continue;
                    }
                    if workspace_is_preempted(cx, workspace) {
                        continue;
                    }
                    crate::ui::tree_sync::resync_window_from_tree(cx, workspace);
                }
            }
            ControlEvent::GuiOpen { workspace, .. } if host.is_local() && workspace.is_some() => {
                let workspace = workspace.expect("guarded above");
                crate::ui::windows::open_named_workspace_from_cli(cx, workspace);
            }
            ControlEvent::GuiOpen { path, .. } if host.is_local() => {
                crate::ui::windows::open_from_cli(cx, path.map(std::path::PathBuf::from));
            }
            other => log::debug!("unhandled control event from {host:?}: {other:?}"),
        }
    }
}

fn reconnect_after_restart(origin: &str, cx: &mut gpui::App) {
    let Some(host) = remote_connect::origin_host(origin) else {
        return;
    };
    remote_connect::HostLinks::remove(cx, host);
    for (workspace, _) in workspaces_on(cx, host) {
        RemoteLinks::retry_now(cx, workspace);
    }
    RemoteLinks::ensure_running(cx);
    cx.refresh_windows();
}

fn client_id_for(cx: &gpui::App, host: HostId, store_key: &str) -> Option<WorkspaceId> {
    workspaces_on(cx, host)
        .into_iter()
        .find(|(_, key)| key == store_key)
        .map(|(id, _)| id)
}

fn launch_attempt(cx: &mut gpui::App, host: HostId, target: RemoteTarget) {
    let label = target.to_string();
    let header = match remote_connect::control_route(&target, cx) {
        Ok(header) => header,
        Err(e) => {
            RemoteLinks::mark(cx, host, |link| {
                link.last_error = Some(e.clone());
                link.state = LinkState::Failed(e);
                link.next_attempt = None;
                link.attempting = false;
            });
            cx.refresh_windows();
            return;
        }
    };
    let keys: Vec<String> = workspaces_on(cx, host)
        .into_iter()
        .map(|(_, key)| key)
        .collect();

    RemoteLinks::mark(cx, host, |link| link.attempting = true);
    let for_finish = target.clone();
    cx.spawn(async move |cx| {
        let label_for_task = label.clone();
        let outcome = cx
            .background_executor()
            .spawn(async move {
                let connected = remote_connect::connect_blocking(&target, header, &label_for_task)?;
                // What the far end was actually told, so the pump does not go
                // on to send the same attach a second time over this link.
                let mut sent: Vec<String> = Vec::new();
                for key in &keys {
                    match connected
                        .host
                        .client()
                        .call(ControlRequest::WorkspaceAttach { id: key.clone() })
                    {
                        Ok(ReplyOk::Attached {
                            took_over_from: Some(who),
                        }) => {
                            log::info!("took workspace {key} back from {who}");
                            sent.push(key.clone());
                        }
                        Ok(_) => sent.push(key.clone()),
                        Err(e) => log::warn!("could not attach to workspace {key}: {e}"),
                    }
                }
                Ok::<_, String>((connected, sent))
            })
            .await;
        cx.update(|cx| finish_attempt(cx, host, &for_finish, outcome));
    })
    .detach();
}

fn finish_attempt(
    cx: &mut gpui::App,
    host: HostId,
    target: &RemoteTarget,
    outcome: Result<(remote_connect::Connected, Vec<String>), String>,
) {
    let label = target.to_string();
    match outcome {
        Ok((connected, sent)) => {
            let restarted = server_restarted(cx, host, &connected.host);
            remote_connect::HostLinks::insert(cx, connected.host, connected.home);
            for (id, key) in workspaces_on(cx, host) {
                let reclaimed = {
                    let links = cx.default_global::<RemoteLinks>();
                    links.preempted.remove(&id).is_some() | links.reclaiming.remove(&id).is_some()
                };
                if sent.contains(&key) {
                    RemoteLinks::mark(cx, host, |link| {
                        link.attach_sent.insert(id);
                    });
                }
                if restarted || reclaimed {
                    crate::ui::tree_sync::resync_window_from_tree(cx, id);
                } else {
                    relink_panes(cx, id);
                    crate::ui::tree_sync::hydrate_window_from_tree(cx, id);
                }
                refresh_window_shells(cx, id);
            }
            RemoteLinks::mark(cx, host, |link| {
                link.state = LinkState::Attached;
                link.backoff.reset();
                link.next_attempt = None;
                link.attempting = false;
                link.last_error = None;
            });
            clear_window_failures_for(cx, target);
            log::info!("reconnected to {label}");
        }
        Err(e) => {
            log::warn!("reconnect to {label} failed: {e}");
            RemoteLinks::mark(cx, host, |link| {
                link.attempting = false;
                link.state = LinkState::Reconnecting;
                link.next_attempt = None;
                link.last_error = Some(e.clone());
            });
        }
    }
    cx.refresh_windows();
}

fn relink_panes(cx: &mut gpui::App, workspace: WorkspaceId) {
    let route = pane_route_for(cx, workspace);
    if matches!(route, crate::terminal::PaneRoute::Local) {
        return;
    }
    let panes = panes_of(cx, workspace);
    if panes.is_empty() {
        return;
    }
    log::info!("relinking {} pane(s) of workspace {workspace}", panes.len());
    for view in panes {
        let (pane_id, size, cell_w, cell_h) = view.read(cx).relink_plan();
        let opening = route.clone();
        let adopting = route.clone();
        cx.spawn(async move |cx| {
            let opened = cx
                .background_executor()
                .spawn(async move {
                    crate::terminal::RemoteTerminal::open_relink(
                        &opening, pane_id, size, cell_w, cell_h,
                    )
                    .map_err(|e| e.to_string())
                })
                .await;
            match opened {
                Ok(stream) => {
                    view.update(cx, |view, cx| {
                        if let Err(e) =
                            view.adopt_relink(stream, &adopting, size, cell_w, cell_h, cx)
                        {
                            log::warn!("pane {pane_id} re-attached but could not be adopted: {e}");
                        }
                    });
                }
                Err(e) => log::warn!("could not relink pane {pane_id}: {e}"),
            }
        })
        .detach();
    }
}

fn server_restarted(cx: &mut gpui::App, host: HostId, peer: &RemoteHost) -> bool {
    let instance = peer.peer().instance.clone();
    let seen = &mut cx.default_global::<RemoteLinks>().instances;
    crate::ui::tree_sync::note_instance(seen.entry(host).or_default(), &instance)
}

fn refresh_window_shells(cx: &mut gpui::App, workspace: WorkspaceId) {
    let Some(app) =
        crate::ui::windows::WindowRegistry::app_for(cx, workspace).and_then(|app| app.upgrade())
    else {
        return;
    };
    app.update(cx, |app, cx| app.refresh_shells(cx));
}

fn release_panes(cx: &mut gpui::App, workspace: WorkspaceId) {
    for view in panes_of(cx, workspace) {
        view.update(cx, |view, cx| view.detach_link(cx));
    }
}

pub(crate) fn pump_auth_sheets(cx: &mut gpui::App) {
    #[cfg(test)]
    let _turn = remote_connect::claim_mailbox();

    let mut inbox: Vec<remote_connect::PendingAuth> = Vec::new();
    while let Some(pending) = remote_connect::take_pending_auth() {
        inbox.push(pending);
    }
    if let Ok(mut parked) = PARKED.lock() {
        inbox.append(&mut parked);
    }

    for pending in inbox {
        let host = pending.host;
        if !cx.default_global::<RemoteLinks>().auth.request(host) {
            park(pending);
            continue;
        }
        match raise_auth_sheet(cx, pending) {
            SheetOutcome::Raised => {}
            outcome => {
                cx.default_global::<RemoteLinks>().auth.release(host);
                if let SheetOutcome::GiveBack(pending) = outcome {
                    park(pending);
                }
            }
        }
    }
}

pub(crate) enum SheetOutcome {
    Raised,
    GiveBack(remote_connect::PendingAuth),
    Lost,
}

fn raise_auth_sheet(cx: &mut gpui::App, pending: remote_connect::PendingAuth) -> SheetOutcome {
    let host = pending.host;
    let Some((workspace, _)) = workspaces_on(cx, host).into_iter().next() else {
        return SheetOutcome::GiveBack(pending);
    };
    let Some(handle) = crate::ui::windows::WindowRegistry::window_for(cx, workspace) else {
        return SheetOutcome::GiveBack(pending);
    };
    let Some(app) =
        crate::ui::windows::WindowRegistry::app_for(cx, workspace).and_then(|app| app.upgrade())
    else {
        return SheetOutcome::GiveBack(pending);
    };
    handle
        .update(cx, move |_, window, cx| {
            app.update(cx, |app, cx| app.raise_routed_auth(pending, window, cx))
        })
        .unwrap_or(SheetOutcome::Lost)
}

pub(crate) fn release_auth_sheet(host: HostId, cx: &mut gpui::App) {
    cx.default_global::<RemoteLinks>().auth.release(host);
}

static PARKED: Mutex<Vec<remote_connect::PendingAuth>> = Mutex::new(Vec::new());

fn park(pending: remote_connect::PendingAuth) {
    if let Ok(mut parked) = PARKED.lock() {
        parked.push(pending);
    }
}

fn panes_of(
    cx: &mut gpui::App,
    workspace: WorkspaceId,
) -> Vec<gpui::Entity<crate::terminal::view::TerminalView>> {
    let Some(app) = crate::ui::windows::WindowRegistry::app_for(cx, workspace) else {
        return Vec::new();
    };
    let Some(app) = app.upgrade() else {
        return Vec::new();
    };
    app.read(cx).panes()
}

#[allow(
    dead_code,
    reason = "the five keystroke entry points that call this live in terminal/view.rs"
)]
pub(crate) fn workspace_accepts_input(cx: &gpui::App, workspace: WorkspaceId) -> bool {
    RemoteLinks::status_of(cx, workspace).is_none_or(|s| s.accepts_input())
}

pub(crate) fn workspace_is_preempted(cx: &gpui::App, workspace: WorkspaceId) -> bool {
    cx.try_global::<RemoteLinks>()
        .is_some_and(|links| links.preempted.contains_key(&workspace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn taking_back_marks_the_workspace_for_a_whole_rebuild(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(crate::core::config::Config::default());
            let host = RemoteRef::new(
                RemoteTarget::Alias {
                    alias: "build-box".into(),
                },
                WorkspaceId::new(),
            );
            let view = crate::core::session::WindowView {
                host: Some(host),
                ..Default::default()
            };
            let id = view.id;
            crate::core::session::WorkspaceStore::install_for_test(
                cx,
                crate::core::session::WindowViews {
                    views: vec![view],
                    active: None,
                },
            );
            cx.default_global::<RemoteLinks>()
                .preempted
                .insert(id, "laptop".into());

            RemoteLinks::retry_now(cx, id);

            let links = cx.default_global::<RemoteLinks>();
            assert!(
                !links.preempted.contains_key(&id),
                "the takeover is being reversed; the read-only state ends now"
            );
            assert!(
                links.reclaiming.contains_key(&id),
                "the attach that lands must know to rebuild this window from the tree"
            );
        });
    }

    #[gpui::test]
    fn a_stopped_supervisor_restarts_when_a_remote_workspace_comes_back(
        cx: &mut gpui::TestAppContext,
    ) {
        let id = cx.update(|cx| {
            cx.set_global(crate::core::config::Config::default());
            crate::core::session::WorkspaceStore::install_for_test(
                cx,
                crate::core::session::WindowViews::default(),
            );
            RemoteLinks::ensure_running(cx);
            assert!(cx.default_global::<RemoteLinks>().pumping);
            WorkspaceId::new()
        });
        cx.background_executor.run_until_parked();
        cx.update(|cx| {
            assert!(
                !cx.default_global::<RemoteLinks>().pumping,
                "with no remote workspace open the pump is expected to stop"
            );

            let host = RemoteRef::new(
                RemoteTarget::Alias {
                    alias: "build-box".into(),
                },
                WorkspaceId::new(),
            );
            crate::core::session::WorkspaceStore::install_for_test(
                cx,
                crate::core::session::WindowViews {
                    views: vec![crate::core::session::WindowView {
                        id,
                        host: Some(host),
                        open: true,
                        ..Default::default()
                    }],
                    active: Some(id),
                },
            );
            RemoteLinks::supervise(cx, id);

            assert!(
                cx.default_global::<RemoteLinks>().pumping,
                "reopening a remote workspace has to start the supervisor again"
            );
        });
    }

    #[gpui::test]
    fn a_plain_reconnect_is_not_marked_for_a_rebuild(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            cx.set_global(crate::core::config::Config::default());
            let host = RemoteRef::new(
                RemoteTarget::Alias {
                    alias: "build-box".into(),
                },
                WorkspaceId::new(),
            );
            let view = crate::core::session::WindowView {
                host: Some(host),
                ..Default::default()
            };
            let id = view.id;
            crate::core::session::WorkspaceStore::install_for_test(
                cx,
                crate::core::session::WindowViews {
                    views: vec![view],
                    active: None,
                },
            );

            RemoteLinks::retry_now(cx, id);

            assert!(
                !cx.default_global::<RemoteLinks>()
                    .reclaiming
                    .contains_key(&id),
                "nothing was taken over, so nothing needs the Replace path"
            );
        });
    }

    #[test]
    fn the_status_strip_speaks_unless_everything_is_working() {
        crate::ui::i18n::set_locale("en");
        assert_eq!(RemoteStatus::Attached.strip_message("build-box"), None);
        assert_eq!(
            RemoteStatus::Disconnected.strip_message("build-box"),
            Some(t_fmt(
                L10nKey::RemoteStripDisconnected,
                &[("machine", "build-box")]
            ))
        );
        assert_eq!(
            RemoteStatus::Connecting.strip_message("build-box"),
            Some(t_fmt(
                L10nKey::RemoteStripConnecting,
                &[("machine", "build-box")]
            ))
        );
        assert_eq!(
            RemoteStatus::Failed("connection refused".into()).strip_message("build-box"),
            Some(t_fmt(
                L10nKey::RemoteStripFailed,
                &[("machine", "build-box"), ("error", "connection refused")]
            ))
        );
    }

    #[test]
    fn input_is_gated_on_being_attached() {
        assert!(RemoteStatus::Attached.accepts_input());
        assert!(!RemoteStatus::Disconnected.accepts_input());
        assert!(!RemoteStatus::Connecting.accepts_input());
        assert!(!RemoteStatus::Failed("x".into()).accepts_input());
    }

    #[test]
    fn every_flow_state_names_its_machine() {
        let choice = HostChoice {
            target: RemoteTarget::Alias {
                alias: "build-box".into(),
            },
            label: "build-box".into(),
            detail: "me@build-box".into(),
        };
        let flow = ConnectFlow::Connecting {
            choice: choice.clone(),
        };
        assert_eq!(flow.choice(), Some(&choice));
        let flow = ConnectFlow::Failed {
            choice: choice.clone(),
            error: "no route to host".into(),
        };
        assert_eq!(flow.choice(), Some(&choice));
    }

    /// What the comparison itself answers is pinned where it now lives, in
    /// `tree_sync`. What is this module's own is the slot it reads: one per
    /// host, keyed the way `server_restarted` keys it, so a machine that
    /// restarted says nothing about the one next to it.
    #[test]
    fn instances_are_per_machine() {
        let mut seen = std::collections::HashMap::new();
        let a = HostId::from_connection_key("ssh:box-a");
        let b = HostId::from_connection_key("ssh:box-b");

        assert!(!crate::ui::tree_sync::note_instance(
            seen.entry(a).or_default(),
            "a1"
        ));
        assert!(!crate::ui::tree_sync::note_instance(
            seen.entry(b).or_default(),
            "b1"
        ));
        assert!(crate::ui::tree_sync::note_instance(
            seen.entry(a).or_default(),
            "a2"
        ));
        assert!(
            !crate::ui::tree_sync::note_instance(seen.entry(b).or_default(), "b1"),
            "box-b never changed; box-a restarting is not its business"
        );
    }

    #[test]
    fn the_backoff_doubles_to_thirty_seconds_and_stays_there() {
        let mut b = Backoff::default();
        let seen: Vec<u64> = (0..8).map(|_| b.advance().as_secs()).collect();
        assert_eq!(seen, vec![1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(b.attempt(), 8, "every attempt is counted, capped or not");
    }

    #[test]
    fn a_success_resets_the_schedule() {
        let mut b = Backoff::default();
        for _ in 0..5 {
            b.advance();
        }
        assert_eq!(b.delay(), RECONNECT_CAP);
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_eq!(b.delay(), RECONNECT_FIRST);
    }

    #[test]
    fn the_backoff_survives_absurd_attempt_counts() {
        let mut b = Backoff::default();
        for _ in 0..200 {
            assert!(b.advance() <= RECONNECT_CAP);
        }
        assert_eq!(b.delay(), RECONNECT_CAP, "still retrying, still capped");
    }

    fn host(key: &str) -> HostId {
        HostId::from_connection_key(key)
    }

    #[test]
    fn only_one_machine_may_raise_a_sheet_at_a_time() {
        let mut q = AuthSheetQueue::default();
        let (a, b, c) = (
            host("ssh-alias:a"),
            host("ssh-alias:b"),
            host("ssh-alias:c"),
        );

        assert!(q.request(a), "the first asker goes straight through");
        assert!(!q.request(b));
        assert!(!q.request(c));
        assert_eq!(q.waiting(), 2);
        assert_eq!(q.holder(), Some(a));

        assert_eq!(q.release(a), Some(b));
        assert_eq!(q.waiting(), 1);
        assert_eq!(q.release(b), Some(c));
        assert_eq!(q.release(c), None);
        assert_eq!(q.holder(), None);
        assert_eq!(q.waiting(), 0);
    }

    #[test]
    fn the_holder_may_ask_again_without_deadlocking() {
        let mut q = AuthSheetQueue::default();
        let a = host("ssh-alias:a");
        assert!(q.request(a));
        assert!(q.request(a));
        assert_eq!(q.waiting(), 0);
    }

    #[test]
    fn a_connect_that_gave_up_leaves_the_queue() {
        let mut q = AuthSheetQueue::default();
        let (a, b, c) = (
            host("ssh-alias:a"),
            host("ssh-alias:b"),
            host("ssh-alias:c"),
        );
        q.request(a);
        q.request(b);
        q.request(c);

        q.withdraw(b);
        assert_eq!(q.waiting(), 1);
        assert_eq!(q.release(a), Some(c), "b is gone, c is next");

        assert_eq!(q.release(b), None);
        assert_eq!(q.holder(), Some(c));
    }

    #[test]
    fn two_windows_on_one_machine_share_a_place_in_the_queue() {
        let mut q = AuthSheetQueue::default();
        let (a, b) = (host("ssh-alias:build"), host("ssh-alias:other"));
        assert!(q.request(a));
        assert!(!q.request(b));
        assert!(q.request(a));
        assert_eq!(q.waiting(), 1);
    }

    #[test]
    fn every_state_says_what_it_means_for_the_keyboard() {
        crate::ui::i18n::set_locale("en");
        let cases = [
            (RemoteStatus::Attached, true, None, None),
            (
                RemoteStatus::Disconnected,
                false,
                Some(t(L10nKey::RemoteNoticeDisconnected)),
                Some(t(L10nKey::RemoteActionConnect)),
            ),
            (
                RemoteStatus::Connecting,
                false,
                Some(t(L10nKey::RemoteNoticeDisconnected)),
                None,
            ),
            (
                RemoteStatus::Reconnecting {
                    attempt: 2,
                    last_error: None,
                },
                false,
                Some(t(L10nKey::RemoteNoticeDisconnected)),
                Some(t(L10nKey::RemoteActionRetryNow)),
            ),
            (
                RemoteStatus::Preempted {
                    by: "desktop".into(),
                },
                false,
                Some(t(L10nKey::RemoteNoticePreempted)),
                Some(t(L10nKey::RemoteActionTakeBack)),
            ),
            (
                RemoteStatus::Failed("no route to host".into()),
                false,
                Some(t(L10nKey::RemoteNoticeDisconnected)),
                Some(t(L10nKey::RemoteActionRetry)),
            ),
        ];
        for (status, accepts, notice, action) in cases {
            assert_eq!(status.accepts_input(), accepts, "{status:?}");
            assert_eq!(status.input_notice(), notice, "{status:?}");
            assert_eq!(status.action_label(), action, "{status:?}");
        }
    }

    #[test]
    fn the_new_states_name_what_happened() {
        crate::ui::i18n::set_locale("en");
        assert_eq!(
            RemoteStatus::Reconnecting {
                attempt: 0,
                last_error: None,
            }
            .strip_message("build-box"),
            Some(t_fmt(
                L10nKey::RemoteStripReconnecting,
                &[("machine", "build-box")]
            )),
            "the first attempt does not need a count"
        );
        assert_eq!(
            RemoteStatus::Reconnecting {
                attempt: 3,
                last_error: None,
            }
            .strip_message("build-box"),
            Some(t_fmt(
                L10nKey::RemoteStripReconnectingAttempt,
                &[("machine", "build-box"), ("count", "4")]
            ))
        );
        assert_eq!(
            RemoteStatus::Preempted {
                by: "desktop".into()
            }
            .strip_message("build-box"),
            Some(t_fmt(L10nKey::RemoteStripPreempted, &[("by", "desktop")]))
        );
    }

    fn machine(alias: &str) -> (HostId, RemoteTarget) {
        let target = RemoteTarget::Alias {
            alias: alias.to_string(),
        };
        (target.host_id(), target)
    }

    #[test]
    fn a_disconnect_ends_when_the_last_window_on_that_machine_closes() {
        let (build, build_t) = machine("build-box");
        let (gpu, gpu_t) = machine("gpu-lab");
        let mut suspended = std::collections::HashSet::from([build, gpu]);

        prune_suspended(&mut suspended, &[(build, build_t.clone()), (gpu, gpu_t)]);
        assert_eq!(suspended.len(), 2);

        prune_suspended(&mut suspended, &[(build, build_t)]);
        assert_eq!(
            suspended.into_iter().collect::<Vec<_>>(),
            vec![build],
            "closing one machine's window must not resume another"
        );
    }

    #[gpui::test]
    fn disconnecting_rests_at_not_connected_and_connect_undoes_it(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::core::config::pin_test_config_dir();
            cx.set_global(crate::core::config::Config::default());
            crate::ui::windows::WindowRegistry::init(cx);

            let (host, target) = machine("build-box");
            let mut entry = crate::core::session::WindowView::on_remote(RemoteRef::new(
                target,
                WorkspaceId::new(),
            ));
            entry.open = true;
            let id = entry.id;
            WorkspaceStore::install_for_test(
                cx,
                crate::core::session::WindowViews {
                    views: vec![entry],
                    active: None,
                },
            );

            RemoteLinks::disconnect(cx, host);
            assert!(cx.default_global::<RemoteLinks>().suspended.contains(&host));
            assert_eq!(
                RemoteLinks::status_of(cx, id),
                Some(RemoteStatus::Disconnected),
                "a disconnected machine rests where a never-connected one does"
            );

            RemoteLinks::retry_now(cx, id);
            assert!(
                !cx.default_global::<RemoteLinks>().suspended.contains(&host),
                "asking to connect must outrank having asked to disconnect"
            );
        });
    }

    #[test]
    fn a_reconnect_says_what_went_wrong_last_time() {
        crate::ui::i18n::set_locale("en");
        assert_eq!(
            RemoteStatus::Reconnecting {
                attempt: 0,
                last_error: Some("connection refused".into()),
            }
            .strip_message("build-box"),
            Some(t_fmt(
                L10nKey::RemoteStripReconnectingWhy,
                &[("machine", "build-box"), ("error", "connection refused")]
            )),
            "the first attempt still needs no count, but it does need a reason"
        );
        assert_eq!(
            RemoteStatus::Reconnecting {
                attempt: 3,
                last_error: Some("connection refused".into()),
            }
            .strip_message("build-box"),
            Some(t_fmt(
                L10nKey::RemoteStripReconnectingAttemptWhy,
                &[
                    ("machine", "build-box"),
                    ("count", "4"),
                    ("error", "connection refused")
                ]
            ))
        );
    }

    fn failed_flow(alias: &str) -> ConnectFlow {
        ConnectFlow::Failed {
            choice: HostChoice {
                target: RemoteTarget::Alias {
                    alias: alias.to_string(),
                },
                label: alias.to_string(),
                detail: alias.to_string(),
            },
            error: "no route to host".into(),
        }
    }

    #[test]
    fn a_live_link_outranks_this_windows_memory_of_a_failed_connect() {
        let (_, target) = machine("build-box");
        let flow = failed_flow("build-box");

        assert_eq!(
            resolve_status(Some(&flow), &target, Some(RemoteStatus::Attached), true),
            Some(RemoteStatus::Attached),
            "the supervisor got through; the failure the window remembers is over"
        );
        assert_eq!(
            resolve_status(
                Some(&flow),
                &target,
                Some(RemoteStatus::Preempted {
                    by: "desktop".into()
                }),
                true
            ),
            Some(RemoteStatus::Preempted {
                by: "desktop".into()
            }),
            "being displaced is news the failed connect cannot answer for"
        );
        assert_eq!(
            resolve_status(Some(&flow), &target, Some(RemoteStatus::Disconnected), true),
            Some(RemoteStatus::Failed("no route to host".into())),
            "with nothing better on offer the window's own failure still stands"
        );
    }

    #[test]
    fn a_route_that_no_longer_resolves_parks_the_workspace() {
        // #485: the profile is gone, so retrying is pointless — but a link
        // that is up, or one someone else has taken, still outranks it: those
        // panes work no matter what happened to the profile that made them.
        let (_, target) = machine("build-box");
        let flow = failed_flow("build-box");

        assert_eq!(
            resolve_status(
                Some(&flow),
                &target,
                Some(RemoteStatus::Disconnected),
                false
            ),
            Some(RemoteStatus::RouteLost),
            "nothing it could try would succeed, so say that instead of retrying"
        );
        assert_eq!(
            resolve_status(None, &target, None, false),
            Some(RemoteStatus::RouteLost),
            "with no connect flow of its own either"
        );
        assert_eq!(
            resolve_status(Some(&flow), &target, Some(RemoteStatus::Attached), false),
            Some(RemoteStatus::Attached),
            "a live link outranks a lost route"
        );
        assert_eq!(
            resolve_status(
                Some(&flow),
                &target,
                Some(RemoteStatus::Preempted {
                    by: "desktop".into()
                }),
                false
            ),
            Some(RemoteStatus::Preempted {
                by: "desktop".into()
            }),
            "so does being displaced — that is news a lost route cannot answer for"
        );
    }

    #[test]
    fn a_failure_on_one_machine_does_not_speak_for_another() {
        let (_, target) = machine("build-box");
        let flow = failed_flow("gpu-lab");

        assert_eq!(
            resolve_status(Some(&flow), &target, Some(RemoteStatus::Attached), true),
            Some(RemoteStatus::Attached)
        );
        assert_eq!(
            resolve_status(Some(&flow), &target, Some(RemoteStatus::Disconnected), true),
            Some(RemoteStatus::Disconnected),
            "a window on the build box has no business showing the GPU box's error"
        );
    }

    /// The whole point of the reclaim pass: a Take Back has to reach the wire
    /// exactly once, and a refusal has to leave the strip where it found it.
    #[gpui::test]
    fn a_take_back_is_sent_once_and_undone_when_it_is_refused(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::core::config::pin_test_config_dir();
            cx.set_global(crate::core::config::Config::default());
            crate::ui::windows::WindowRegistry::init(cx);

            let (host, target) = machine("build-box");
            let mut entry = crate::core::session::WindowView::on_remote(RemoteRef::new(
                target,
                WorkspaceId::new(),
            ));
            entry.open = true;
            let id = entry.id;
            WorkspaceStore::install_for_test(
                cx,
                crate::core::session::WindowViews {
                    views: vec![entry],
                    active: None,
                },
            );
            cx.default_global::<RemoteLinks>()
                .preempted
                .insert(id, "desktop".into());

            // Nothing is due while someone else holds it: taking it back is the
            // user's call.
            assert!(reclaims_due(cx, host).is_empty());

            RemoteLinks::retry_now(cx, id);
            assert_eq!(
                reclaims_due(cx, host).len(),
                1,
                "the Take Back has to reach the far end"
            );
            assert!(
                reclaims_due(cx, host).is_empty(),
                "and it has to reach it once, not once every pump tick"
            );
            assert_eq!(
                RemoteLinks::status_of(cx, id),
                Some(RemoteStatus::Connecting),
                "a reclaim in flight is neither taken over nor attached"
            );

            finish_reclaim(cx, host, id, Err("workspace is busy".into()));
            let links = cx.default_global::<RemoteLinks>();
            assert_eq!(
                links.preempted.get(&id).map(String::as_str),
                Some("desktop"),
                "the refusal puts the takeover back, with the name that came with it"
            );
            assert!(!links.attaching.contains(&id));
            assert!(!links.reclaiming.contains_key(&id));
            assert_eq!(
                links.machines.get(&host).and_then(|l| l.last_error.clone()),
                Some("workspace is busy".into())
            );
        });
    }

    #[gpui::test]
    fn a_reclaim_that_lands_leaves_nothing_behind(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::core::config::pin_test_config_dir();
            cx.set_global(crate::core::config::Config::default());
            crate::ui::windows::WindowRegistry::init(cx);

            let (host, target) = machine("build-box");
            let mut entry = crate::core::session::WindowView::on_remote(RemoteRef::new(
                target,
                WorkspaceId::new(),
            ));
            entry.open = true;
            let id = entry.id;
            WorkspaceStore::install_for_test(
                cx,
                crate::core::session::WindowViews {
                    views: vec![entry],
                    active: None,
                },
            );
            cx.default_global::<RemoteLinks>()
                .preempted
                .insert(id, "desktop".into());
            RemoteLinks::retry_now(cx, id);
            assert_eq!(reclaims_due(cx, host).len(), 1);

            finish_reclaim(
                cx,
                host,
                id,
                Ok(ReplyOk::Attached {
                    took_over_from: Some("desktop".into()),
                }),
            );

            let links = cx.default_global::<RemoteLinks>();
            assert!(!links.preempted.contains_key(&id));
            assert!(!links.reclaiming.contains_key(&id));
            assert!(!links.attaching.contains(&id));
            assert!(
                links
                    .machines
                    .get(&host)
                    .is_some_and(|l| l.attach_sent.contains(&id)),
                "the far end has been told; this link needs no second attach"
            );
            assert!(reclaims_due(cx, host).is_empty());
        });
    }

    /// A link the switcher brought up never sends `WorkspaceAttach` of its own,
    /// so the pass that does has to notice a workspace it has never spoken for.
    #[gpui::test]
    fn a_workspace_never_attached_on_this_link_is_due(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::core::config::pin_test_config_dir();
            cx.set_global(crate::core::config::Config::default());
            crate::ui::windows::WindowRegistry::init(cx);

            let (host, target) = machine("build-box");
            let mut entry = crate::core::session::WindowView::on_remote(RemoteRef::new(
                target,
                WorkspaceId::new(),
            ));
            entry.open = true;
            let id = entry.id;
            WorkspaceStore::install_for_test(
                cx,
                crate::core::session::WindowViews {
                    views: vec![entry],
                    active: None,
                },
            );

            assert_eq!(
                reclaims_due(cx, host),
                workspaces_on(cx, host),
                "nothing has been said to this machine yet, so everything open on it is due"
            );
            finish_reclaim(
                cx,
                host,
                id,
                Ok(ReplyOk::Attached {
                    took_over_from: None,
                }),
            );
            assert!(
                reclaims_due(cx, host).is_empty(),
                "one attach per workspace per link"
            );
        });
    }
}

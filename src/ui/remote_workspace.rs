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
    Reconnecting { attempt: u32 },
    Preempted { by: String },
    Failed(String),
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
            RemoteStatus::Reconnecting { attempt: 0 } => Some(t_fmt(
                L10nKey::RemoteStripReconnecting,
                &[("machine", machine)],
            )),
            RemoteStatus::Reconnecting { attempt } => Some(t_fmt(
                L10nKey::RemoteStripReconnectingAttempt,
                &[("machine", machine), ("count", &(attempt + 1).to_string())],
            )),
            RemoteStatus::Preempted { by } => {
                Some(t_fmt(L10nKey::RemoteStripPreempted, &[("by", by)]))
            }
            RemoteStatus::Failed(e) => Some(t_fmt(
                L10nKey::RemoteStripFailed,
                &[("machine", machine), ("error", e)],
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
            Some(host) => host.target.to_string(),
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
        WorkspaceStore::remote_ref(cx, self.workspace)?;
        match &self.connect {
            Some(ConnectFlow::Connecting { .. }) => return Some(RemoteStatus::Connecting),
            Some(ConnectFlow::Failed { error, .. }) => {
                return Some(RemoteStatus::Failed(error.clone()));
            }
            _ => {}
        }
        RemoteLinks::status_of(cx, self.workspace)
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
            &[t(L10nKey::Cancel), t(L10nKey::SettingsInstall)],
            cx,
        );
        cx.spawn(async move |_, _| {
            let decision = match answer.await {
                Ok(1) => InstallDecision::Approve,
                _ => InstallDecision::Decline,
            };
            pending.answer(decision);
        })
        .detach();
    }

    pub(crate) fn prompt_remote_daemon_mismatch(window: &mut Window, cx: &mut Context<Self>) {
        for mismatch in crate::daemon::install::take_mismatched_remote_daemons() {
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
                if !matches!(answer.await, Ok(1)) {
                    return;
                }
                let _ = this.update_in(cx, |this, window, cx| {
                    this.restart_mismatched_remote_server(mismatch, window, cx);
                });
            })
            .detach();
        }
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
            &[t(L10nKey::Cancel), t(L10nKey::RestartServer)],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if !matches!(answer.await, Ok(1)) {
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
            &[t(L10nKey::Cancel), t(L10nKey::RemoteMismatchReplaceServer)],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if !matches!(answer.await, Ok(1)) {
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LinkState {
    Connecting,
    Attached,
    Reconnecting,
    Failed(String),
}

#[derive(Default)]
pub(crate) struct RemoteLinks {
    machines: std::collections::HashMap<HostId, MachineLink>,
    preempted: std::collections::HashMap<WorkspaceId, String>,
    reclaiming: std::collections::HashSet<WorkspaceId>,
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
        if let Some(by) = links.preempted.get(&workspace) {
            return Some(RemoteStatus::Preempted { by: by.clone() });
        }
        Some(match links.machines.get(&host.host_id()) {
            Some(link) => match &link.state {
                LinkState::Connecting => RemoteStatus::Connecting,
                LinkState::Attached => RemoteStatus::Attached,
                LinkState::Reconnecting => RemoteStatus::Reconnecting {
                    attempt: link.backoff.attempt(),
                },
                LinkState::Failed(e) => RemoteStatus::Failed(e.clone()),
            },
            None => RemoteStatus::Disconnected,
        })
    }

    pub(crate) fn retry_now(cx: &mut gpui::App, workspace: WorkspaceId) {
        let Some(host) = WorkspaceStore::remote_ref(cx, workspace) else {
            return;
        };
        let links = cx.default_global::<RemoteLinks>();
        if links.preempted.remove(&workspace).is_some() {
            links.reclaiming.insert(workspace);
        }
        links.suspended.remove(&host.host_id());
        let link = links.machines.entry(host.host_id()).or_insert(MachineLink {
            state: LinkState::Reconnecting,
            backoff: Backoff::default(),
            next_attempt: None,
            attempting: false,
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
            });
            if became {
                changed = true;
                log::info!("link to {target} is attached");
                crate::ui::machine_mirror::MachineMirrors::refresh(cx, host);
            }
            continue;
        }
        if attempting {
            continue;
        }

        if remote_connect::HostLinks::get(cx, host).is_some() {
            remote_connect::HostLinks::remove(cx, host);
            log::info!("lost the control connection to {target}; reconnecting");
        }

        let due = {
            let mut due = false;
            RemoteLinks::mark(cx, host, |link| {
                if !matches!(link.state, LinkState::Reconnecting) {
                    changed = true;
                    link.state = LinkState::Reconnecting;
                }
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

fn prune_suspended(
    suspended: &mut std::collections::HashSet<HostId>,
    bound: &[(HostId, RemoteTarget)],
) {
    suspended.retain(|host| bound.iter().any(|(id, _)| id == host));
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
    cx.spawn(async move |cx| {
        let label_for_task = label.clone();
        let outcome = cx
            .background_executor()
            .spawn(async move {
                let connected = remote_connect::connect_blocking(&target, header, &label_for_task)?;
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
                        }
                        Ok(_) => {}
                        Err(e) => log::warn!("could not attach to workspace {key}: {e}"),
                    }
                }
                Ok::<_, String>(connected)
            })
            .await;
        cx.update(|cx| finish_attempt(cx, host, &label, outcome));
    })
    .detach();
}

fn finish_attempt(
    cx: &mut gpui::App,
    host: HostId,
    label: &str,
    outcome: Result<remote_connect::Connected, String>,
) {
    match outcome {
        Ok(connected) => {
            let restarted = server_restarted(cx, host, &connected.host);
            remote_connect::HostLinks::insert(cx, connected.host, connected.home);
            for (id, _key) in workspaces_on(cx, host) {
                let reclaimed = {
                    let links = cx.default_global::<RemoteLinks>();
                    links.preempted.remove(&id).is_some() | links.reclaiming.remove(&id)
                };
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
            });
            log::info!("reconnected to {label}");
        }
        Err(e) => {
            log::warn!("reconnect to {label} failed: {e}");
            RemoteLinks::mark(cx, host, |link| {
                link.attempting = false;
                link.state = LinkState::Reconnecting;
                link.next_attempt = None;
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
    note_instance(seen, host, &instance)
}

fn note_instance(
    seen: &mut std::collections::HashMap<HostId, String>,
    host: HostId,
    instance: &str,
) -> bool {
    if instance.is_empty() {
        return false;
    }
    match seen.insert(host, instance.to_string()) {
        Some(before) if before != instance => {
            log::info!(
                "the tty7-server on this machine is a new process ({before} → {instance}); \
                 its panes are gone"
            );
            true
        }
        _ => false,
    }
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
                links.reclaiming.contains(&id),
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
                !cx.default_global::<RemoteLinks>().reclaiming.contains(&id),
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

    #[test]
    fn only_a_changed_instance_counts_as_a_restart() {
        let mut seen = std::collections::HashMap::new();
        let host = HostId::from_connection_key("ssh:build-box");

        assert!(!note_instance(&mut seen, host, "abc"));
        assert!(!note_instance(&mut seen, host, "abc"));
        assert!(note_instance(&mut seen, host, "def"));
        assert!(!note_instance(&mut seen, host, "def"));
    }

    #[test]
    fn an_unknown_instance_is_not_a_restart_and_is_not_remembered() {
        let mut seen = std::collections::HashMap::new();
        let host = HostId::from_connection_key("ssh:build-box");

        assert!(!note_instance(&mut seen, host, ""));
        assert!(seen.is_empty(), "an unknown instance must not be recorded");
        assert!(
            !note_instance(&mut seen, host, "abc"),
            "the first real instance is a first sighting, not a restart"
        );
    }

    #[test]
    fn instances_are_per_machine() {
        let mut seen = std::collections::HashMap::new();
        let a = HostId::from_connection_key("ssh:box-a");
        let b = HostId::from_connection_key("ssh:box-b");

        assert!(!note_instance(&mut seen, a, "a1"));
        assert!(!note_instance(&mut seen, b, "b1"));
        assert!(note_instance(&mut seen, a, "a2"));
        assert!(
            !note_instance(&mut seen, b, "b1"),
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
                RemoteStatus::Reconnecting { attempt: 2 },
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
            RemoteStatus::Reconnecting { attempt: 0 }.strip_message("build-box"),
            Some(t_fmt(
                L10nKey::RemoteStripReconnecting,
                &[("machine", "build-box")]
            )),
            "the first attempt does not need a count"
        );
        assert_eq!(
            RemoteStatus::Reconnecting { attempt: 3 }.strip_message("build-box"),
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
}

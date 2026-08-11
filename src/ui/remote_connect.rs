use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, BorrowAppContext as _, Global};

use crate::core::config::Config;
use crate::core::session::{RemoteRef, RemoteTarget, WorkspaceId};
use crate::daemon::control::{ControlHello, ControlRequest, ReplyOk};
use crate::daemon::install::{
    InstallConfirm, InstallDecision, InstallPhase, InstallProgress, InstallRequest,
    MismatchedRemoteDaemon,
};
use crate::daemon::protocol::{AuthPromptKind, AuthResponse, NativeSshSpec};
use crate::daemon::router::RouteHeader;
use crate::ui::i18n::{L10nKey, t, t_fmt};
use tty7_core::host::remote::RemoteHost;
use tty7_core::host::{Host as _, HostId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostChoice {
    pub target: RemoteTarget,
    pub label: String,
    pub detail: String,
}

pub fn available_hosts(cx: &App) -> Vec<HostChoice> {
    let mut out: Vec<HostChoice> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    out.extend(local_stdio_host());

    for profile in &cx.global::<Config>().ssh_profiles {
        let target = RemoteTarget::Profile { id: profile.id };
        seen.push(profile.name.clone());
        out.push(HostChoice {
            detail: endpoint_label(&profile.user, &profile.host, profile.port),
            label: profile_label(profile),
            target,
        });
    }

    out.extend(wsl_hosts(cx));

    for imported in crate::core::ssh_config::import_profiles() {
        let alias = imported.profile.name.clone();
        if alias.trim().is_empty() || seen.iter().any(|s| s == &alias) {
            continue;
        }
        out.push(HostChoice {
            detail: endpoint_label(
                &imported.profile.user,
                &imported.profile.host,
                imported.profile.port,
            ),
            label: alias,
            target: RemoteTarget::Alias {
                alias: imported.profile.name,
            },
        });
    }
    out
}

/// A label for a bare target when no route snapshot is at hand (a pane's
/// `PaneWorkspace` carries only the target): the live listing while it
/// exists, the target's own spelling when that is human-readable, and the
/// deleted-profile placeholder for the bare-UUID case (#485).
pub fn target_label(cx: &App, target: &RemoteTarget) -> String {
    if let Some(choice) = available_hosts(cx)
        .into_iter()
        .find(|h| h.target == *target)
    {
        return choice.label;
    }
    match target {
        RemoteTarget::Profile { .. } => t(L10nKey::RemoteProfileGone).to_string(),
        other => other.to_string(),
    }
}

/// Whether a route can still be turned into a connection: a `Profile` target
/// dangles once the profile leaves the config, an `Alias` once the name
/// leaves `~/.ssh/config` (#485). The route supervisor asks this four times a
/// second; the alias half is mtime-cached in `ssh_config`.
pub fn route_resolvable(cx: &App, target: &RemoteTarget) -> bool {
    target.resolvable(&cx.global::<Config>().ssh_profiles, |alias| {
        crate::core::ssh_config::alias_still_resolves(alias)
    })
}

/// The name a route entry answers to: the live listing while the route still
/// exists, then the remembered snapshot, and — for a profile entry saved
/// before snapshots existed — a placeholder, but never the bare profile UUID
/// (#485).
pub fn route_label(cx: &App, host: &RemoteRef) -> String {
    match live_label(cx, &host.target) {
        Some(label) => label,
        None => host.route_label(t(L10nKey::RemoteProfileGone)),
    }
}

/// What the live config calls a target, reading memory only. Deliberately not
/// `available_hosts`: `route_label` runs on the render path of every remote
/// window (the workspace strip), and that listing re-reads `~/.ssh/config`
/// off the disk. The two targets it answers are the two whose name lives
/// somewhere other than the target itself; the rest spell themselves the same
/// way the listing labels them, so falling through costs nothing.
fn live_label(cx: &App, target: &RemoteTarget) -> Option<String> {
    match target {
        RemoteTarget::Profile { id } => cx
            .global::<Config>()
            .ssh_profiles
            .iter()
            .find(|p| p.id == *id)
            .map(profile_label),
        RemoteTarget::Wsl { .. } => wsl_hosts(cx)
            .into_iter()
            .find(|h| h.target == *target)
            .map(|h| h.label),
        RemoteTarget::Alias { .. }
        | RemoteTarget::Direct { .. }
        | RemoteTarget::LocalStdio { .. } => None,
    }
}

/// What a profile calls itself in a host listing: its name, or the address
/// when it was saved without one.
fn profile_label(profile: &crate::core::ssh_profile::SshProfile) -> String {
    if profile.name.trim().is_empty() {
        profile.host.clone()
    } else {
        profile.name.clone()
    }
}

pub fn filter_hosts(hosts: &[HostChoice], query: &str) -> Vec<HostChoice> {
    let query = query.trim();
    if query.is_empty() {
        return hosts.to_vec();
    }
    let mut scored: Vec<(i32, &HostChoice)> = hosts
        .iter()
        .filter_map(|host| host_score(query, host).map(|score| (score, host)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, host)| host.clone()).collect()
}

fn host_score(query: &str, host: &HostChoice) -> Option<i32> {
    let label = crate::ui::palette::fuzzy_score(query, &host.label);
    let detail = crate::ui::palette::fuzzy_score(query, &host.detail).map(|score| score - 3);
    label.into_iter().chain(detail).max()
}

pub const LOCAL_STDIO_ENV: &str = "TTY7_LOCAL_STDIO_SERVER";

fn local_stdio_host() -> Option<HostChoice> {
    let program = std::env::var(LOCAL_STDIO_ENV)
        .ok()
        .filter(|p| !p.is_empty())?;
    let target = RemoteTarget::LocalStdio {
        program: program.clone(),
        args: vec!["--stdio".to_string()],
    };
    Some(HostChoice {
        label: format!("{target}"),
        detail: format!("{program} --stdio ({})", t(L10nKey::RemoteThisComputer)),
        target,
    })
}

fn wsl_detail() -> String {
    format!("WSL · {}", t(L10nKey::RemoteThisComputer))
}

fn wsl_hosts(cx: &App) -> Vec<HostChoice> {
    let names = cx
        .try_global::<WslDistros>()
        .map(|state| state.names.as_slice())
        .unwrap_or_default();
    wsl_choices(names)
}

fn wsl_choices(names: &[String]) -> Vec<HostChoice> {
    names
        .iter()
        .map(|distro| HostChoice {
            target: RemoteTarget::Wsl {
                distro: distro.clone(),
            },
            label: distro.clone(),
            detail: wsl_detail(),
        })
        .collect()
}

#[derive(Default)]
struct WslDistros {
    names: Vec<String>,
    probed_at: Option<Instant>,
    in_flight: bool,
}

impl Global for WslDistros {}

const WSL_TTL: Duration = Duration::from_secs(30);

pub fn sweep_wsl(cx: &mut App) {
    if !cfg!(windows) {
        return;
    }
    {
        let state = cx.default_global::<WslDistros>();
        if state.in_flight || state.probed_at.is_some_and(|at| at.elapsed() < WSL_TTL) {
            return;
        }
    }
    cx.update_global::<WslDistros, _>(|state, _| state.in_flight = true);
    cx.spawn(async move |cx| {
        let probed = cx
            .background_spawn(async { crate::core::shells::wsl_distros_probed() })
            .await;
        let _ = cx.update(|cx| {
            cx.update_global::<WslDistros, _>(|state, _| adopt_probe(state, probed));
            cx.refresh_windows();
        });
    })
    .detach();
}

fn adopt_probe(state: &mut WslDistros, probed: Option<Vec<String>>) {
    if let Some(names) = probed {
        state.names = names;
    }
    state.probed_at = Some(Instant::now());
    state.in_flight = false;
}

fn endpoint_label(user: &str, host: &str, port: u16) -> String {
    let base = if user.is_empty() {
        host.to_string()
    } else {
        format!("{user}@{host}")
    };
    if port == 22 {
        base
    } else {
        format!("{base}:{port}")
    }
}

pub fn spec_for(target: &RemoteTarget, cx: &App) -> Result<NativeSshSpec, String> {
    let cfg = cx.global::<Config>();
    match target {
        RemoteTarget::Profile { id } => {
            let profile = cfg
                .ssh_profiles
                .iter()
                .find(|p| p.id == *id)
                .ok_or_else(|| t(L10nKey::RemoteProfileMissing).to_string())?;
            Ok(crate::ui::ssh_connect::build_native_ssh_spec(
                profile,
                &cfg.ssh_profiles,
                &crate::core::keychain::OsCredentialStore,
                cfg.verify_host_keys,
            ))
        }
        RemoteTarget::Alias { alias } => {
            let resolved = crate::core::ssh_config::resolve_alias_to_profile(alias)
                .ok_or_else(|| t_fmt(L10nKey::RemoteAliasMissing, &[("alias", alias)]))?;
            Ok(crate::ui::ssh_connect::native_spec_from_transient_profile(
                &resolved.profile,
                resolved.proxy_jump,
                &crate::core::keychain::OsCredentialStore,
                cfg.verify_host_keys,
                &crate::ui::ssh_connect::config_alias_resolver,
            ))
        }
        RemoteTarget::Direct { user, host, port } => {
            let mut profile = crate::core::ssh_profile::SshProfile::new(host.clone());
            profile.host = host.clone();
            profile.user = user.clone();
            profile.port = *port;
            Ok(crate::ui::ssh_connect::build_native_ssh_spec(
                &profile,
                &cfg.ssh_profiles,
                &crate::core::keychain::OsCredentialStore,
                cfg.verify_host_keys,
            ))
        }
        RemoteTarget::Wsl { .. } => Err(t(L10nKey::RemoteWslNoSsh).to_string()),
        RemoteTarget::LocalStdio { .. } => Err(t(L10nKey::RemoteLocalStdioNoSsh).to_string()),
    }
}

pub fn control_route(target: &RemoteTarget, cx: &App) -> Result<RouteHeader, String> {
    let header = match target {
        RemoteTarget::LocalStdio { program, args } => RouteHeader::local_stdio(
            program.clone(),
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        ),
        RemoteTarget::Wsl { distro } => RouteHeader::wsl(distro.clone()),
        _ => spec_for(target, cx).map(RouteHeader::ssh)?,
    };
    note_origin(&header.target, target);
    Ok(header)
}

pub struct Connected {
    pub host: Arc<RemoteHost>,
    pub home: PathBuf,
    pub rows: Vec<RemoteWorkspaceRow>,
}

pub fn connect_blocking(
    target: &RemoteTarget,
    header: RouteHeader,
    label: &str,
) -> Result<Connected, String> {
    note_origin(&header.target, target);
    crate::daemon::spawn::ensure_running().map_err(|e| {
        t_fmt(
            L10nKey::RemoteDaemonStartFailed,
            &[("error", &e.to_string())],
        )
    })?;

    let stream = crate::daemon::transport::connect().map_err(|e| {
        t_fmt(
            L10nKey::RemoteDaemonUnreachable,
            &[("error", &e.to_string())],
        )
    })?;

    let mut stream = stream;
    crate::daemon::router::negotiate(&mut stream, &header).map_err(|e| {
        t_fmt(
            L10nKey::RemoteHostUnreachable,
            &[("machine", label), ("error", &e.to_string())],
        )
    })?;

    let hello = ControlHello::host_rpc(new_session_token(), client_hostname());
    let host = handshake(stream, &target.connection_key(), &hello).map_err(|e| {
        t_fmt(
            L10nKey::RemoteHostNotTty7,
            &[("machine", label), ("error", &e.to_string())],
        )
    })?;

    let rows = list_workspaces(&host).map_err(|e| {
        t_fmt(
            L10nKey::RemoteWorkspaceListFailed,
            &[("machine", label), ("error", &e.to_string())],
        )
    })?;
    let home = host.home();
    refresh_agent_hooks_once(&host, &home);
    Ok(Connected { host, home, rows })
}

static HOOKS_REFRESHED: Mutex<Vec<HostId>> = Mutex::new(Vec::new());

fn refresh_agent_hooks_once(host: &Arc<RemoteHost>, home: &std::path::Path) {
    let id = host.id();
    match HOOKS_REFRESHED.lock() {
        Ok(mut seen) if !seen.contains(&id) => seen.push(id),
        _ => return,
    }
    let (host, home) = (Arc::clone(host), home.to_path_buf());
    std::thread::spawn(move || {
        let refreshed = crate::core::agent_hooks::refresh_remote_hooks(&*host, home);
        if refreshed > 0 {
            log::info!("refreshed {refreshed} stale agent hook integration(s) on {id:?}");
        }
    });
}

#[cfg(unix)]
fn handshake(
    stream: crate::daemon::transport::Stream,
    connection_key: &str,
    hello: &ControlHello,
) -> io::Result<Arc<RemoteHost>> {
    RemoteHost::over_unix(stream, connection_key, hello)
}

#[cfg(windows)]
fn handshake(
    stream: crate::daemon::transport::Stream,
    connection_key: &str,
    hello: &ControlHello,
) -> io::Result<Arc<RemoteHost>> {
    RemoteHost::over_tcp(stream, connection_key, hello)
}

pub fn list_workspaces(host: &Arc<RemoteHost>) -> io::Result<Vec<RemoteWorkspaceRow>> {
    match host.client().call(ControlRequest::MachineGet)? {
        ReplyOk::MachineTree(machine) => Ok(rows_from_machine(&machine)),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            t_fmt(
                L10nKey::RemoteMachineTreeUnexpectedReply,
                &[("reply", &format!("{other:?}"))],
            ),
        )),
    }
}

fn new_session_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn client_hostname() -> String {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "a tty7 client".to_string())
    })
    .clone()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteWorkspaceRow {
    pub id: WorkspaceId,
    pub name: String,
    pub panes: usize,
    pub last_active: u64,
}

pub fn rows_from_machine(machine: &tty7_core::core::machine::Machine) -> Vec<RemoteWorkspaceRow> {
    let mut rows: Vec<RemoteWorkspaceRow> = machine
        .workspaces
        .iter()
        .map(|ws| RemoteWorkspaceRow {
            id: ws.id,
            name: crate::ui::machine_mirror::display_name_of(ws, &machine.panes),
            panes: ws.tabs.iter().map(|t| t.root.pane_ids().len()).sum(),
            last_active: ws.last_active,
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.last_active));
    rows
}

#[derive(Default)]
pub struct HostLinks {
    hosts: HashMap<HostId, Arc<RemoteHost>>,
    homes: HashMap<HostId, PathBuf>,
}

impl Global for HostLinks {}

impl HostLinks {
    pub fn get(cx: &mut App, id: HostId) -> Option<Arc<RemoteHost>> {
        cx.default_global::<HostLinks>().hosts.get(&id).cloned()
    }

    pub fn home(cx: &mut App, id: HostId) -> Option<PathBuf> {
        cx.default_global::<HostLinks>().homes.get(&id).cloned()
    }

    pub fn insert(cx: &mut App, host: Arc<RemoteHost>, home: PathBuf) {
        let id = host.id();
        crate::ui::host_registry::HostRegistry::insert(cx, Arc::clone(&host).into_shared());
        let table = cx.default_global::<HostLinks>();
        table.hosts.insert(id, host);
        table.homes.insert(id, home);
    }

    pub fn remove(cx: &mut App, id: HostId) {
        let table = cx.default_global::<HostLinks>();
        table.hosts.remove(&id);
        table.homes.remove(&id);
        crate::ui::host_registry::HostRegistry::remove(cx, id);
    }

    pub fn len(cx: &mut App) -> usize {
        cx.default_global::<HostLinks>().hosts.len()
    }
}

pub fn install_detail(request: &InstallRequest) -> String {
    t_fmt(
        L10nKey::RemoteInstallDetail,
        &[
            ("machine", &request.host),
            ("path", &request.remote_path),
            (
                "version",
                &format!("{} ({})", request.version, request.asset),
            ),
            ("size", &human_bytes(request.size_bytes)),
            ("from", &request.source_url),
            ("sha256", &request.sha256),
            ("path_label", t(L10nKey::RemoteInstallPathLabel)),
            ("version_label", t(L10nKey::RemoteInstallVersionLabel)),
            ("size_label", t(L10nKey::RemoteInstallSizeLabel)),
            ("from_label", t(L10nKey::RemoteInstallFromLabel)),
            ("sha_label", t(L10nKey::RemoteInstallShaLabel)),
            ("silent_upgrades", t(L10nKey::RemoteInstallSilentUpgrades)),
        ],
    )
}

pub fn install_title(request: &InstallRequest) -> String {
    t_fmt(L10nKey::RemoteInstallTitle, &[("machine", &request.host)])
}

pub fn human_bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    let n = n as f64;
    if n < KIB {
        return format!("{} {}", n as u64, t(L10nKey::RemoteInstallBytes));
    }
    let units = ["KiB", "MiB", "GiB"];
    let mut value = n / KIB;
    for (i, unit) in units.iter().enumerate() {
        if value < KIB || i == units.len() - 1 {
            return format!("{value:.1} {unit}");
        }
        value /= KIB;
    }
    unreachable!("the loop returns on its last iteration")
}

const CONSENT_TIMEOUT: Duration = Duration::from_secs(180);

pub struct PendingInstall {
    pub request: InstallRequest,
    reply: std::sync::mpsc::SyncSender<InstallDecision>,
}

impl PendingInstall {
    pub fn answer(self, decision: InstallDecision) {
        let _ = self.reply.send(decision);
    }
}

static MAILBOX: Mutex<Vec<PendingInstall>> = Mutex::new(Vec::new());

pub struct GuiInstallConfirm;

impl InstallConfirm for GuiInstallConfirm {
    fn confirm(&self, request: &InstallRequest) -> InstallDecision {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        {
            let Ok(mut mailbox) = MAILBOX.lock() else {
                return InstallDecision::Decline;
            };
            mailbox.push(PendingInstall {
                request: request.clone(),
                reply: tx,
            });
        }
        rx.recv_timeout(CONSENT_TIMEOUT)
            .unwrap_or(InstallDecision::Decline)
    }
}

static PROGRESS: Mutex<Vec<(HostId, InstallPhase)>> = Mutex::new(Vec::new());

pub struct GuiInstallProgress;

impl InstallProgress for GuiInstallProgress {
    fn report(&self, host: &str, phase: InstallPhase) {
        let id = origin_host(host).unwrap_or_else(|| HostId::from_connection_key(host));
        let Ok(mut slots) = PROGRESS.lock() else {
            return;
        };
        match slots.iter_mut().find(|(known, _)| *known == id) {
            Some(slot) => slot.1 = phase,
            None => slots.push((id, phase)),
        }
    }
}

pub fn install_progress_for(host: HostId) -> Option<InstallPhase> {
    let slots = PROGRESS.lock().ok()?;
    slots
        .iter()
        .find(|(known, _)| *known == host)
        .map(|(_, phase)| *phase)
}

pub fn clear_install_progress(host: HostId) {
    if let Ok(mut slots) = PROGRESS.lock() {
        slots.retain(|(known, _)| *known != host);
    }
}

pub fn register(cx: &mut App) {
    crate::daemon::install::set_install_confirm(Arc::new(GuiInstallConfirm));
    crate::daemon::install::set_install_progress(Arc::new(GuiInstallProgress));
    crate::daemon::router::set_route_auth_responder(Arc::new(GuiRouteAuth));
    let _ = HostLinks::len(cx);
}

pub fn take_pending_install() -> Option<PendingInstall> {
    MAILBOX.lock().ok()?.pop()
}

pub struct PendingAuth {
    pub host: HostId,
    pub prompt: AuthPromptKind,
    /// Which connection is asking, when the route is an SSH hop. The sheet
    /// files and forgets keychain entries under this; a route that is not SSH
    /// (WSL, a local stdio server) has no endpoint to name and gets `None`.
    ///
    /// Without it the sheet fell back to port 22 and a hard-coded "not
    /// auto-supplied", so a routed prompt for a non-22 endpoint wrote its
    /// password under the wrong key and a rejected stored one was never
    /// noticed, let alone cleared.
    pub endpoint: Option<crate::ui::ssh_prompt::PromptEndpoint>,
    /// The route already carried a stored password into this attempt, so a
    /// password prompt arriving anyway means the server turned it down.
    pub auto_supplied_password: bool,
    reply: std::sync::mpsc::SyncSender<AuthResponse>,
}

impl PendingAuth {
    pub fn answer(self, response: AuthResponse) {
        let _ = self.reply.send(response);
    }
}

static AUTH_MAILBOX: Mutex<Vec<PendingAuth>> = Mutex::new(Vec::new());

struct RouteOrigin {
    key: String,
    target: RemoteTarget,
    host: HostId,
}

static ORIGINS: Mutex<Vec<RouteOrigin>> = Mutex::new(Vec::new());

pub fn note_origin(route: &crate::daemon::router::RouteTarget, target: &RemoteTarget) {
    let key = route.origin_key();
    let Ok(mut origins) = ORIGINS.lock() else {
        return;
    };
    let host = target.host_id();
    match origins.iter_mut().find(|o| o.key == key) {
        Some(existing) => {
            existing.target = target.clone();
            existing.host = host;
        }
        None => origins.push(RouteOrigin {
            key,
            target: target.clone(),
            host,
        }),
    }
}

pub fn origin_host(key: &str) -> Option<HostId> {
    let origins = ORIGINS.lock().ok()?;
    origins.iter().find(|o| o.key == key).map(|o| o.host)
}

pub fn origin_target(key: &str) -> Option<RemoteTarget> {
    let origins = ORIGINS.lock().ok()?;
    origins
        .iter()
        .find(|o| o.key == key)
        .map(|o| o.target.clone())
}

pub struct GuiRouteAuth;

impl crate::daemon::router::RouteAuthResponder for GuiRouteAuth {
    fn respond(
        &self,
        machine: &crate::daemon::router::RouteTarget,
        prompt: &AuthPromptKind,
    ) -> AuthResponse {
        let key = machine.origin_key();
        let host = origin_host(&key).unwrap_or_else(|| HostId::from_connection_key(&key));
        let (endpoint, auto_supplied_password) = match machine {
            crate::daemon::router::RouteTarget::Ssh(spec) => (
                Some(crate::ui::ssh_prompt::PromptEndpoint {
                    user: spec.user.clone(),
                    host: spec.host.clone(),
                    port: spec.port,
                }),
                spec.password.is_some(),
            ),
            _ => (None, false),
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        {
            let Ok(mut mailbox) = AUTH_MAILBOX.lock() else {
                return AuthResponse::Cancelled;
            };
            mailbox.push(PendingAuth {
                host,
                prompt: prompt.clone(),
                endpoint,
                auto_supplied_password,
                reply: tx,
            });
        }
        rx.recv_timeout(CONSENT_TIMEOUT)
            .unwrap_or(AuthResponse::Cancelled)
    }
}

pub fn take_pending_auth() -> Option<PendingAuth> {
    AUTH_MAILBOX.lock().ok()?.pop()
}

#[cfg(test)]
pub(crate) static MAILBOX_TURN: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn claim_mailbox() -> std::sync::MutexGuard<'static, ()> {
    MAILBOX_TURN.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn mismatch_answers() -> [gpui::PromptButton; 2] {
    crate::ui::confirm_answers(t(L10nKey::RemoteMismatchReplaceServer), t(L10nKey::Cancel))
}

pub fn mismatch_detail(m: &MismatchedRemoteDaemon) -> String {
    let running = match (&m.running_version, &m.running_exe) {
        (Some(v), Some(exe)) => t_fmt(
            L10nKey::RemoteMismatchVersionFromExe,
            &[("version", v), ("exe", exe)],
        ),
        (Some(v), None) => v.clone(),
        (None, Some(exe)) => t_fmt(L10nKey::RemoteMismatchUnknownBuildFromExe, &[("exe", exe)]),
        (None, None) => t(L10nKey::RemoteMismatchUnknownBuild).to_string(),
    };
    t_fmt(
        L10nKey::RemoteMismatchDetail,
        &[
            ("machine", &m.host),
            ("running", &running),
            ("wanted", &m.wanted_version),
            ("replace_server", t(L10nKey::RemoteMismatchReplaceServer)),
            ("cancel", t(L10nKey::Cancel)),
        ],
    )
}

pub fn mismatch_title(m: &MismatchedRemoteDaemon) -> String {
    t_fmt(L10nKey::RemoteMismatchTitle, &[("machine", &m.host)])
}

pub fn mismatch_target(m: &MismatchedRemoteDaemon) -> Option<RemoteTarget> {
    origin_target(&m.host)
}

pub fn restart_server_blocking(header: RouteHeader, label: &str) -> Result<(), String> {
    let action = header.action;
    crate::daemon::spawn::ensure_running().map_err(|e| {
        t_fmt(
            L10nKey::RemoteDaemonStartFailed,
            &[("error", &e.to_string())],
        )
    })?;
    let mut stream = crate::daemon::transport::connect().map_err(|e| {
        t_fmt(
            L10nKey::RemoteDaemonUnreachable,
            &[("error", &e.to_string())],
        )
    })?;
    let ack = crate::daemon::router::negotiate(&mut stream, &header).map_err(|e| {
        t_fmt(
            L10nKey::RemoteServerRestartFailed,
            &[("machine", label), ("error", &e.to_string())],
        )
    })?;
    if !ack.performed(action) {
        return Err(t_fmt(L10nKey::RemoteDaemonTooOld, &[("machine", label)]));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> InstallRequest {
        InstallRequest {
            host: "me@build-box:22".into(),
            version: "0.9.1".into(),
            asset: "tty7-server-linux-x86_64-musl",
            source_url: "https://example.invalid/v0.9.1/tty7-server".into(),
            remote_path: "/home/me/.local/share/tty7/bin/tty7-server-0.9.1".into(),
            size_bytes: 9_437_184,
            sha256: "abc123".into(),
        }
    }

    #[test]
    fn the_install_prompt_states_every_field_of_the_request() {
        crate::ui::i18n::set_locale("en");
        let request = request();
        let detail = install_detail(&request);
        for needle in [
            request.remote_path.as_str(),
            request.version.as_str(),
            request.asset,
            request.source_url.as_str(),
            request.sha256.as_str(),
        ] {
            assert!(
                detail.contains(needle),
                "{needle:?} missing from:\n{detail}"
            );
        }
        assert!(detail.contains("9.0 MiB"), "{detail}");
        assert_eq!(
            install_title(&request),
            t_fmt(
                L10nKey::RemoteInstallTitle,
                &[("machine", "me@build-box:22")]
            )
        );
    }

    #[test]
    fn human_bytes_reads_in_binary_units() {
        crate::ui::i18n::set_locale("en");
        assert_eq!(
            human_bytes(512),
            format!("512 {}", t(L10nKey::RemoteInstallBytes))
        );
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1_572_864), "1.5 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn an_unanswered_install_request_declines() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let pending = PendingInstall {
            request: request(),
            reply: tx,
        };
        drop(pending);
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn answering_an_install_request_delivers_the_decision() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        PendingInstall {
            request: request(),
            reply: tx,
        }
        .answer(InstallDecision::Approve);
        assert_eq!(rx.recv().unwrap(), InstallDecision::Approve);
    }

    #[test]
    fn the_gui_handler_parks_the_request_for_the_ui_to_answer() {
        while take_pending_install().is_some() {}
        let handle = std::thread::spawn(|| GuiInstallConfirm.confirm(&request()));
        let pending = loop {
            if let Some(p) = take_pending_install() {
                break p;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(pending.request.host, "me@build-box:22");
        assert!(take_pending_install().is_none(), "handed out twice");
        pending.answer(InstallDecision::Approve);
        assert_eq!(handle.join().unwrap(), InstallDecision::Approve);
    }

    fn native_spec(user: &str, host: &str, port: u16) -> NativeSshSpec {
        let mut profile = crate::core::ssh_profile::SshProfile::new(host.to_string());
        profile.user = user.to_string();
        profile.port = port;
        crate::ui::ssh_connect::build_native_ssh_spec(
            &profile,
            &[],
            &crate::core::keychain::InMemoryCredentialStore::new(),
            false,
        )
    }

    #[test]
    fn a_routed_auth_prompt_carries_the_machine_that_raised_it() {
        let _turn = claim_mailbox();
        while take_pending_auth().is_some() {}
        let target = RemoteTarget::direct("me", "build-box", 22);
        let route =
            crate::daemon::router::RouteTarget::Ssh(Box::new(native_spec("me", "build-box", 22)));
        note_origin(&route, &target);

        let handle = std::thread::spawn(move || {
            use crate::daemon::router::RouteAuthResponder as _;
            GuiRouteAuth.respond(
                &route,
                &AuthPromptKind::Password {
                    user: "me".into(),
                    host: "build-box".into(),
                },
            )
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let pending = loop {
            if let Some(p) = take_pending_auth() {
                break p;
            }
            assert!(
                Instant::now() < deadline,
                "no routed prompt arrived within 10s. `respond` pushes one \
                 unconditionally, so an empty mailbox means something else \
                 drained it first — `pump_auth_sheets` takes all of it, and it \
                 runs from any gpui test here that drives a tick. Responder \
                 thread finished: {}",
                handle.is_finished(),
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(pending.host, target.host_id());
        pending.answer(AuthResponse::Secret("hunter2".into()));
        assert_eq!(
            handle.join().unwrap(),
            AuthResponse::Secret("hunter2".into())
        );
    }

    #[test]
    fn a_pane_and_its_workspace_resolve_to_the_same_machine() {
        use crate::daemon::router::{RouteHeader, RouteTarget as RT};

        let target = RemoteTarget::direct("me", "twin-box", 22);
        let control = RouteHeader::ssh(native_spec("me", "twin-box", 22));
        let pane = RouteHeader::ssh(native_spec("me", "twin-box", 22)).for_pane();
        note_origin(&control.target, &target);
        assert_eq!(
            origin_host(&pane.target.origin_key()),
            Some(target.host_id()),
            "a pane's header names the machine its workspace's does"
        );

        let local = RemoteTarget::LocalStdio {
            program: "/opt/tty7-server".into(),
            args: vec!["--stdio".into()],
        };
        let control = RT::LocalStdio {
            program: "/opt/tty7-server".into(),
            args: vec!["--stdio".into()],
        };
        let pane = RT::LocalStdio {
            program: "/opt/tty7-server".into(),
            args: vec!["--stdio".into(), "--pane".into()],
        };
        note_origin(&control, &local);
        assert_eq!(origin_host(&pane.origin_key()), Some(local.host_id()));
    }

    #[test]
    fn a_mismatch_record_resolves_back_to_the_machine_it_is_about() {
        let target = RemoteTarget::direct("me", "skew-box", 2222);
        let spec = native_spec("me", "skew-box", 2222);
        let label = crate::daemon::ssh::ConnectionKey::from_spec(&spec)
            .as_str()
            .to_string();
        note_origin(
            &crate::daemon::router::RouteTarget::Ssh(Box::new(spec)),
            &target,
        );

        let mismatch = MismatchedRemoteDaemon {
            host: label,
            running_version: Some("0.8.0".into()),
            running_exe: None,
            wanted_version: "0.9.1".into(),
        };
        assert_eq!(mismatch_target(&mismatch), Some(target));

        assert_eq!(
            mismatch_target(&MismatchedRemoteDaemon {
                host: "me@never-seen:22".into(),
                ..mismatch
            }),
            None
        );
    }

    #[test]
    fn the_mismatch_prompt_names_the_host_and_both_versions() {
        crate::ui::i18n::set_locale("en");
        let m = MismatchedRemoteDaemon {
            host: "me@build-box:22".into(),
            running_version: Some("0.8.0".into()),
            running_exe: Some("/home/me/.local/share/tty7/bin/tty7-server-0.8.0".into()),
            wanted_version: "0.9.1".into(),
        };
        let detail = mismatch_detail(&m);
        assert!(detail.contains("0.8.0"), "{detail}");
        assert!(detail.contains("0.9.1"), "{detail}");
        assert!(detail.contains("me@build-box:22"), "{detail}");
        assert_eq!(
            mismatch_title(&m),
            t_fmt(
                L10nKey::RemoteMismatchTitle,
                &[("machine", "me@build-box:22")]
            )
        );

        let unknown = MismatchedRemoteDaemon {
            running_version: None,
            running_exe: None,
            ..m
        };
        assert!(
            mismatch_detail(&unknown).contains(t(L10nKey::RemoteMismatchUnknownBuild)),
            "{detail}"
        );
    }

    #[test]
    fn the_mismatch_detail_explains_every_answer_the_prompt_offers() {
        crate::ui::i18n::set_locale("en");
        let detail = mismatch_detail(&MismatchedRemoteDaemon {
            host: "me@build-box:22".into(),
            running_version: Some("0.8.0".into()),
            running_exe: None,
            wanted_version: "0.9.1".into(),
        });
        for answer in mismatch_answers() {
            let label = answer.label();
            assert!(
                detail.contains(label.as_ref()),
                "{label} is unexplained: {detail}"
            );
        }
    }

    #[test]
    fn endpoint_labels_hide_the_default_port() {
        assert_eq!(endpoint_label("me", "box.local", 22), "me@box.local");
        assert_eq!(endpoint_label("me", "box.local", 2222), "me@box.local:2222");
        assert_eq!(endpoint_label("", "box.local", 22), "box.local");
    }

    #[test]
    fn rows_from_the_tree_sort_newest_first_and_derive_names() {
        use tty7_core::core::machine::{Machine, PaneRecord, Tab, Workspace};
        let older = WorkspaceId::new();
        let newer = WorkspaceId::new();
        let machine = Machine {
            workspaces: vec![
                Workspace {
                    id: older,
                    name: Some("api".into()),
                    last_active: 100,
                    tabs: vec![Tab::leaf(1)],
                    ..Default::default()
                },
                Workspace {
                    id: newer,
                    name: None,
                    last_active: 500,
                    tabs: vec![Tab::leaf(2)],
                    ..Default::default()
                },
            ],
            panes: vec![
                PaneRecord::new(1),
                PaneRecord {
                    cwd: Some("/srv/checkout".into()),
                    ..PaneRecord::new(2)
                },
            ],
        };
        let rows = rows_from_machine(&machine);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, newer, "newest first");
        assert_eq!(
            rows[0].name, "checkout",
            "no user name falls back to the first pane's directory"
        );
        assert_eq!(rows[0].panes, 1);
        assert_eq!(rows[1].name, "api", "a user-set name wins");
    }

    fn host(label: &str, detail: &str) -> HostChoice {
        HostChoice {
            target: RemoteTarget::Alias {
                alias: label.to_string(),
            },
            label: label.to_string(),
            detail: detail.to_string(),
        }
    }

    #[test]
    fn an_empty_query_keeps_every_machine_in_order() {
        let hosts = vec![
            host("gate2jup", "root@18.143.92.244"),
            host("aws-xy", "root@52.199.113.213"),
        ];
        let all = filter_hosts(&hosts, "   ");
        assert_eq!(all, hosts);
    }

    #[test]
    fn a_query_matches_the_name_or_the_endpoint() {
        let hosts = vec![
            host("gate2jup", "root@18.143.92.244"),
            host("aws-xy", "root@52.199.113.213"),
            host("orb", "default@127.0.0.1:32222"),
        ];

        let by_name = filter_hosts(&hosts, "jup");
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].label, "gate2jup");

        let by_address = filter_hosts(&hosts, "52.199");
        assert_eq!(by_address.len(), 1);
        assert_eq!(by_address[0].label, "aws-xy");

        let mixed = filter_hosts(&hosts, "or");
        assert_eq!(mixed[0].label, "orb", "a name match beats an endpoint one");
    }

    #[test]
    fn a_query_nothing_matches_returns_nothing() {
        let hosts = vec![host("gate2jup", "root@18.143.92.244")];
        assert!(filter_hosts(&hosts, "zzz").is_empty());
    }

    #[test]
    fn a_wsl_row_names_the_distro_verbatim() {
        let rows = wsl_choices(&["Ubuntu-22.04".to_string(), "Arch".to_string()]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "Ubuntu-22.04");
        assert_eq!(
            rows[0].target,
            RemoteTarget::Wsl {
                distro: "Ubuntu-22.04".to_string()
            }
        );
        assert_eq!(rows[0].target.connection_key(), "wsl:Ubuntu-22.04");
        assert_eq!(rows[1].label, "Arch");
    }

    #[test]
    fn no_distros_is_no_rows() {
        assert!(wsl_choices(&[]).is_empty());
    }

    #[test]
    fn a_distro_is_found_by_name_or_by_wsl() {
        let mut hosts = vec![host("gate2jup", "root@18.143.92.244")];
        hosts.extend(wsl_choices(&["Ubuntu".to_string()]));

        let by_name = filter_hosts(&hosts, "ubun");
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].label, "Ubuntu");

        let by_kind = filter_hosts(&hosts, "wsl");
        assert_eq!(by_kind.len(), 1);
        assert_eq!(by_kind[0].label, "Ubuntu");
    }

    #[test]
    fn a_failed_probe_keeps_the_distros_it_already_had() {
        let mut state = WslDistros {
            names: vec!["Ubuntu-24.04".to_string()],
            ..Default::default()
        };
        adopt_probe(&mut state, None);

        assert_eq!(state.names, vec!["Ubuntu-24.04".to_string()]);
        assert!(state.probed_at.is_some(), "the TTL still restarts");
        assert!(!state.in_flight, "the next sweep is allowed to run");
    }

    #[test]
    fn an_answered_probe_replaces_the_list_even_when_it_is_empty() {
        let mut state = WslDistros {
            names: vec!["Ubuntu-24.04".to_string()],
            ..Default::default()
        };
        adopt_probe(&mut state, Some(Vec::new()));
        assert!(state.names.is_empty());

        adopt_probe(&mut state, Some(vec!["Arch".to_string()]));
        assert_eq!(state.names, vec!["Arch".to_string()]);
    }
}

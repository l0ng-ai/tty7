//! "Connect to Host": the client half of a remote workspace.
//!
//! This module is everything between *the user picked a machine* and *the window
//! is bound to a workspace on it*. It has no gpui views of its own — the home
//! page renders the panels (`ui::home`) and `Tty7App` owns the state
//! (`ui::app`) — because every step here is blocking work that has to happen off
//! the UI thread, and keeping it out of the render path is what makes that
//! obvious.
//!
//! ## The five steps
//!
//! | | Step | Here |
//! |---|---|---|
//! | 1 | List the machines the user already configured | [`available_hosts`] |
//! | 2 | Resolve one into a self-contained SSH spec | [`spec_for`] |
//! | 3 | Open a routed control connection through the local daemon | [`connect_blocking`] |
//! | 4 | Read the machine's own workspace list | [`rows_from_list`] |
//! | 5 | Hold the connection for the workspaces bound to it | [`HostLinks`] |
//!
//! ## Machines are configured once
//!
//! A remote workspace reuses an SSH configuration that already
//! exists — a saved profile or a `~/.ssh/config` alias — with its keys, its jump
//! host and its `ProxyCommand` already set up. There is deliberately no host
//! *editor* here; [`available_hosts`] only reads, and [`spec_for`] hands the
//! resolution straight to `ui::ssh_connect`, which is the same code the SSH-pane
//! entry points use. A machine reachable as an SSH pane is reachable as a
//! workspace, with nothing to configure twice and nothing to keep in step.
//!
//! ## …except WSL, which is configured zero times
//!
//! A WSL distribution is the one machine with nothing to
//! configure: it is reached by spawning `wsl.exe -d <distro> -- tty7-server
//! --stdio`, so there is no address, no credential and no host key — nothing
//! that could be set up, and nothing that could be set up wrongly. So it is not
//! read from a store like the other rows but *discovered*, by [`sweep_wsl`],
//! and every distro the user has installed is offered.
//!
//! ## The connection is routed, not direct
//!
//! Design D3: the GUI never speaks SSH. It opens the same local daemon socket it
//! always did and prefixes one `RouteHeader` frame; the daemon opens the SSH
//! channel and copies bytes. So the sequence in [`connect_blocking`] is
//! *local socket → route header → route ack → control hello*, and only the last
//! of those is a conversation with the remote machine.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, BorrowAppContext as _, Global};

use crate::core::config::Config;
use crate::core::session::{RemoteTarget, WorkspaceId};
use crate::daemon::control::{ControlHello, ControlRequest, ReplyOk};
use crate::daemon::install::{
    InstallConfirm, InstallDecision, InstallPhase, InstallProgress, InstallRequest,
    MismatchedRemoteDaemon,
};
use crate::daemon::protocol::{AuthPromptKind, AuthResponse, NativeSshSpec};
use crate::daemon::router::RouteHeader;
use tty7_core::host::remote::RemoteHost;
use tty7_core::host::{Host as _, HostId};

// ---------------------------------------------------------------------------
// 1. What there is to connect to
// ---------------------------------------------------------------------------

/// One machine the user has already configured, ready to be offered.
///
/// `label` is the name they gave it; `detail` is the endpoint, so two profiles
/// pointing at the same box are told apart by the line that actually differs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostChoice {
    pub target: RemoteTarget,
    pub label: String,
    pub detail: String,
}

/// Every machine tty7 already knows how to reach: saved profiles first, then
/// this computer's WSL distributions, then `~/.ssh/config` aliases.
///
/// Profiles come first because they are the ones the user built deliberately;
/// the config aliases are a long tail that is often machine-generated. An alias
/// whose name matches a profile is dropped rather than listed twice — the
/// profile carries strictly more (credentials, forwards), so it is the better
/// of the two rows and the duplicate would only make the list longer.
///
/// Distributions sit between the two: installing one is as deliberate as
/// writing a profile, but the list is *discovered* rather than written, so it
/// does not outrank the machines the user named by hand.
pub fn available_hosts(cx: &App) -> Vec<HostChoice> {
    let mut out: Vec<HostChoice> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    out.extend(local_stdio_host());

    for profile in &cx.global::<Config>().ssh_profiles {
        let target = RemoteTarget::Profile { id: profile.id };
        seen.push(profile.name.clone());
        out.push(HostChoice {
            detail: endpoint_label(&profile.user, &profile.host, profile.port),
            label: if profile.name.trim().is_empty() {
                profile.host.clone()
            } else {
                profile.name.clone()
            },
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

/// The name the picker shows for `target`.
///
/// Not `RemoteTarget`'s `Display`, which for a saved profile is its *uuid* — the
/// type deliberately cannot reach into the profile store, so anything putting a
/// machine's name in front of the user has to do this lookup. Falls back to the
/// `Display` for a machine no longer on file, which is the honest answer: that
/// is all tty7 still knows about it.
pub fn label_for(target: &RemoteTarget, cx: &App) -> String {
    available_hosts(cx)
        .into_iter()
        .find(|host| host.target == *target)
        .map(|host| host.label)
        .unwrap_or_else(|| target.to_string())
}

/// The machines matching `query`, best match first.
///
/// A `~/.ssh/config` with fifty `Host` blocks is normal, and a list that long
/// is not something anyone reads — it is something they search. So the picker
/// filters instead of scrolling to the letter `w`.
///
/// An empty query keeps [`available_hosts`]'s own order (profiles first, then
/// aliases): that order is deliberate, and a score has nothing to add to it. A
/// non-empty one is the palette's fuzzy match over the name, falling back to the
/// endpoint at a penalty — an alias is how the user thinks of a box, but
/// "the one on 10.0.0.4" is a real way to look for one too.
pub fn filter_hosts(hosts: &[HostChoice], query: &str) -> Vec<HostChoice> {
    let query = query.trim();
    if query.is_empty() {
        return hosts.to_vec();
    }
    let mut scored: Vec<(i32, &HostChoice)> = hosts
        .iter()
        .filter_map(|host| host_score(query, host).map(|score| (score, host)))
        .collect();
    // Stable, so machines that score the same stay in the order above.
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, host)| host.clone()).collect()
}

/// How well `host` answers `query`: its name, or its endpoint at a penalty
/// (`-3`, the same one the command palette puts on a subtitle match).
fn host_score(query: &str, host: &HostChoice) -> Option<i32> {
    let label = crate::ui::palette::fuzzy_score(query, &host.label);
    let detail = crate::ui::palette::fuzzy_score(query, &host.detail).map(|score| score - 3);
    label.into_iter().chain(detail).max()
}

/// The environment variable that stands a machine up on this computer.
///
/// Set it to a `tty7-server` binary and the picker grows one extra row that
/// routes through [`RemoteTarget::LocalStdio`] — a real remote workspace, with a
/// real server process, a real control handshake and real routed panes, and no
/// sshd anywhere. It is the only way to exercise the whole path by hand, and
/// the same seam `crates/tty7-server/tests/routed_pane.rs` uses.
///
/// Deliberately an environment variable and not a setting: it is a developer's
/// tool, and a row in Settings would be a feature nobody outside this repo
/// should ever see.
pub const LOCAL_STDIO_ENV: &str = "TTY7_LOCAL_STDIO_SERVER";

/// The dev-only "machine on this computer" row, when the environment asks for
/// one. Empty in every normal run, which is why nothing downstream needs to
/// know it exists.
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
        detail: format!("{program} --stdio (this computer)"),
        target,
    })
}

/// What the endpoint column says for a distribution. It has no address to
/// print, so it says what kind of machine it is and where it is instead — and
/// it doubles as the thing a user typing `wsl` into the search box matches,
/// since [`host_score`] falls back to this line.
const WSL_DETAIL: &str = "WSL · this computer";

/// This computer's WSL distributions, as machines a workspace can live on.
///
/// Reads [`WslDistros`] and never probes: this runs inside the switcher's
/// render, and enumerating distros spawns a process. [`sweep_wsl`] is what
/// fills it.
fn wsl_hosts(cx: &App) -> Vec<HostChoice> {
    let names = cx
        .try_global::<WslDistros>()
        .map(|state| state.names.as_slice())
        .unwrap_or_default();
    wsl_choices(names)
}

/// The pure half of [`wsl_hosts`]: distro names in, rows out.
fn wsl_choices(names: &[String]) -> Vec<HostChoice> {
    names
        .iter()
        .map(|distro| HostChoice {
            target: RemoteTarget::Wsl {
                distro: distro.clone(),
            },
            // The distro name *is* what the user calls it — `wsl -d` takes this
            // exact string — so there is no friendlier name to look up.
            label: distro.clone(),
            detail: WSL_DETAIL.to_string(),
        })
        .collect()
}

/// This computer's WSL distributions, as of the last probe.
///
/// A global filled in the background rather than a call, because
/// [`available_hosts`] runs inside the switcher's render and `wsl.exe -l -q` is
/// a process spawn — a beat on a warm distribution, much worse on a cold one.
/// The same shape `terminal::pane_liveness` uses for the machine answers drawn
/// two rows above these.
#[derive(Default)]
struct WslDistros {
    /// The last list a probe actually produced. A probe that could not answer
    /// leaves it alone — see [`sweep_wsl`].
    names: Vec<String>,
    /// When the last probe landed; `None` while none ever has, which is what
    /// makes the first [`sweep_wsl`] run instead of waiting out the TTL.
    probed_at: Option<Instant>,
    /// One probe at a time: the switcher can render many frames inside the
    /// couple of hundred milliseconds `wsl.exe` takes to answer.
    in_flight: bool,
}

impl Global for WslDistros {}

/// How long a distribution list is trusted. Installing or unregistering one is
/// rare and deliberate, so this is not a poll — it is short enough that someone
/// who just ran `wsl --install` finds their distro by reopening the switcher
/// rather than by restarting tty7.
const WSL_TTL: Duration = Duration::from_secs(30);

/// Re-enumerate this computer's WSL distributions if the list is missing or
/// stale.
///
/// Safe to call from `render` or from an action: it reads the global, may start
/// background work, and never blocks. Off Windows there are no distributions
/// and nothing is spawned.
///
/// **A probe that could not answer keeps the last list.** `wsl.exe` refuses while
/// a `wsl --shutdown` is in flight, and overwriting with its empty answer would
/// make every distribution vanish from the switcher for a TTL — over something
/// the user runs routinely and that changed nothing. An *authoritative* empty
/// answer still clears the rows, which is what unregistering the last
/// distribution has to look like.
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
            // The frame that asked for this list is long gone — the answer lands
            // on a background task, and an idle switcher has nothing else that
            // would redraw it.
            cx.refresh_windows();
        });
    })
    .detach();
}

/// Fold a probe result into the state: an answer replaces the list, a probe that
/// could not answer leaves it standing, and either way the stamp advances so the
/// TTL governs the next attempt.
///
/// Pure, because this is the whole judgement in [`sweep_wsl`] and the rest of it
/// is a background task on a platform CI cannot run.
fn adopt_probe(state: &mut WslDistros, probed: Option<Vec<String>>) {
    if let Some(names) = probed {
        state.names = names;
    }
    state.probed_at = Some(Instant::now());
    state.in_flight = false;
}

/// `user@host` — with the port only when it isn't the default, which is the
/// convention every other endpoint line in the app follows.
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

// ---------------------------------------------------------------------------
// 2. Resolving a choice into a spec
// ---------------------------------------------------------------------------

/// Resolve a [`RemoteTarget`] into the self-contained spec the daemon needs —
/// secrets, jump chain and all — through the same path an SSH pane uses.
///
/// `Err` is a user-facing sentence: a target can name a profile that has since
/// been deleted or an alias that is no longer in `~/.ssh/config`, and a
/// workspace pointing at one has to say so rather than fail as a connect error
/// much later.
pub fn spec_for(target: &RemoteTarget, cx: &App) -> Result<NativeSshSpec, String> {
    let cfg = cx.global::<Config>();
    match target {
        RemoteTarget::Profile { id } => {
            let profile = cfg
                .ssh_profiles
                .iter()
                .find(|p| p.id == *id)
                .ok_or_else(|| "that saved SSH profile no longer exists".to_string())?;
            Ok(crate::ui::ssh_connect::build_native_ssh_spec(
                profile,
                &cfg.ssh_profiles,
                &crate::core::keychain::OsCredentialStore,
                cfg.verify_host_keys,
            ))
        }
        RemoteTarget::Alias { alias } => {
            let resolved = crate::core::ssh_config::resolve_alias_to_profile(alias)
                .ok_or_else(|| format!("`{alias}` is no longer in ~/.ssh/config"))?;
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
        // Both of these address their machine directly; there is no SSH
        // connection to describe, which is exactly what `Err` means to
        // [`control_route`] and
        // [`crate::terminal::PaneWorkspace::route_header`] — they read the
        // target instead and build a `wsl:` / `stdio:` header from it.
        RemoteTarget::Wsl { .. } => Err("a WSL workspace has no SSH connection".to_string()),
        RemoteTarget::LocalStdio { .. } => {
            Err("a local --stdio workspace has no SSH connection".to_string())
        }
    }
}

/// The route header a *control* connection to `target` opens with.
///
/// The workspace-level twin of
/// [`PaneWorkspace::route_header`](crate::terminal::PaneWorkspace::route_header),
/// and it has to agree with it: the control stream and the pane streams of one
/// workspace must resolve to the same machine, or the window lists one box's
/// files while its terminals run on another.
pub fn control_route(target: &RemoteTarget, cx: &App) -> Result<RouteHeader, String> {
    let header = match target {
        RemoteTarget::LocalStdio { program, args } => RouteHeader::local_stdio(
            program.clone(),
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        ),
        RemoteTarget::Wsl { distro } => RouteHeader::wsl(distro.clone()),
        _ => spec_for(target, cx).map(RouteHeader::ssh)?,
    };
    // Every header this client writes teaches [`RouteOrigins`] one machine, so a
    // question the daemon relays back about it can be attributed without the
    // daemon having to know this client's names for things.
    note_origin(&header.target, target);
    Ok(header)
}

// ---------------------------------------------------------------------------
// 3. Connecting
// ---------------------------------------------------------------------------

/// A machine that answered: its host object, and the workspaces it says it has.
pub struct Connected {
    pub host: Arc<RemoteHost>,
    /// The remote's `$HOME` — where a *new* workspace starts. The
    /// remote's, never this client's.
    pub home: PathBuf,
    pub rows: Vec<RemoteWorkspaceRow>,
}

/// Reach `target` and read its workspace list. **Blocking**; call from a
/// background task.
///
/// The four hops, in order, each with its own failure sentence — a connect that
/// fails has to say *which* of them gave up, because "local daemon isn't
/// running" and "that machine refused the connection" want completely different
/// things from the user:
///
/// 1. the local daemon is running (`spawn::ensure_running`)
/// 2. a local socket to it, carrying a [`RouteHeader`]
/// 3. the daemon's [`RouteAck`] — the SSH connect and channel open happened here
/// 4. the control handshake with the remote `tty7-server`
pub fn connect_blocking(
    target: &RemoteTarget,
    header: RouteHeader,
    label: &str,
) -> Result<Connected, String> {
    // Which machine a relayed question belongs to travels on the header itself
    // (`RouteTarget::origin_key`), so nothing about this thread has to be true
    // for [`GuiRouteAuth`] to name the right host.
    note_origin(&header.target, target);
    crate::daemon::spawn::ensure_running()
        .map_err(|e| format!("tty7's local daemon could not be started: {e}"))?;

    let stream = crate::daemon::transport::connect()
        .map_err(|e| format!("could not reach tty7's local daemon: {e}"))?;

    // `negotiate` writes the header and then *answers* — install consent, an
    // auth prompt, a build-mismatch notice — until the daemon acks. Those
    // questions are raised in the daemon process, which has the connection but
    // no user; this is the end of the socket that has one. Before the relay
    // existed a host needing any of them simply failed here.
    //
    // The ack carries the daemon's own reason when the SSH side failed — a
    // refused connection, a rejected key, an unreachable jump host. Passing it
    // through verbatim is the whole point of the ack existing.
    let mut stream = stream;
    crate::daemon::router::negotiate(&mut stream, &header)
        .map_err(|e| format!("could not reach {label}: {e}"))?;

    let hello = ControlHello::host_rpc(new_session_token(), client_hostname());
    let host = handshake(stream, &target.connection_key(), &hello)
        .map_err(|e| format!("{label} answered, but not as a tty7 server: {e}"))?;

    let rows = list_workspaces(&host)
        .map_err(|e| format!("connected to {label}, but its workspace list failed: {e}"))?;
    let home = host.home();
    refresh_agent_hooks_once(&host, &home);
    Ok(Connected { host, home, rows })
}

/// Machines whose agent hooks this process has already looked at.
static HOOKS_REFRESHED: Mutex<Vec<HostId>> = Mutex::new(Vec::new());

/// Heal this machine's stale tty7 agent hooks — the ones naming a server binary
/// that is no longer the one this client installs, whether because a wire break
/// moved the name or because an older, version-naming client wrote them (see
/// [`crate::core::agent_hooks::refresh_remote_hooks`]).
///
/// Off the connect's own thread, and once per machine per run: it is a config
/// read per agent over the control connection, and a reconnect — which happens
/// on a backoff loop — must not wait on six round trips to a box that may be an
/// ocean away. The hooks are for panes that do not exist yet at this point in
/// the connect, so nothing is racing it.
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

/// The control handshake over the routed stream. Split out only because the
/// transport type differs per platform (a Unix socket here, a token-checked
/// loopback socket on Windows) and both need their shutdown wired so dropping
/// the host actually closes the link.
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

/// Ask a connected machine for its workspaces.
pub fn list_workspaces(host: &Arc<RemoteHost>) -> io::Result<Vec<RemoteWorkspaceRow>> {
    match host.client().call(ControlRequest::MachineGet)? {
        ReplyOk::MachineTree(machine) => Ok(rows_from_machine(&machine)),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the server answered a machine tree with {other:?}"),
        )),
    }
}

/// A one-off session token. The takeover (M6) decides between two
/// clients by this plus the hostname; until then it is only carried.
fn new_session_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// This computer's name, as the remote should show it in a takeover notice.
///
/// Memoized: it cannot change while the process runs, and the lookup shells out.
/// A machine that will not say its name is not an error — `"a tty7 client"` is a
/// worse label but a perfectly serviceable one, and failing a connect over it
/// would be absurd.
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

// ---------------------------------------------------------------------------
// 4. Reading the remote's workspace records
// ---------------------------------------------------------------------------

/// One workspace as the remote machine describes it, flattened for the picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteWorkspaceRow {
    pub id: WorkspaceId,
    /// The user-set name when there is one, else derived from the tabs' repo
    /// groups and cwds — the same precedence `Workspace::display_name` gives a
    /// local workspace, computed here from the machine's tree.
    pub name: String,
    pub panes: usize,
    pub last_active: u64,
}

/// Turn a machine's tree into picker rows, newest first.
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

// ---------------------------------------------------------------------------
// 5. Holding the connections
// ---------------------------------------------------------------------------

/// The live control links, by [`HostId`] — one per machine, one machine per
/// entry.
///
/// The name says the model: every machine this client talks to is reached
/// over exactly one control link, and the local machine is a machine like any
/// other — its link simply lives in its own global
/// ([`LocalLink`](crate::ui::local_link::LocalLink)) because it is in-process
/// rather than wire-backed. One entry per *machine*, not per workspace — the
/// same granularity the SSH connection is pooled at and the same one
/// [`crate::ui::host_registry`] uses, so two windows on one box share a
/// connection, a host object and a git-status cache. This table holds the
/// concrete [`RemoteHost`] because pushing a layout needs its control client;
/// `HostRegistry` holds the same object erased to `dyn Host` for the panels.
#[derive(Default)]
pub struct HostLinks {
    hosts: HashMap<HostId, Arc<RemoteHost>>,
    /// Each machine's `$HOME`, as its handshake reported it.
    ///
    /// Kept beside the connection because it is the same lifetime and the same
    /// scope: it is a fact about the *machine*, true for every window, and it
    /// arrives on the same handshake. It used to live only in the window-owned
    /// `Tty7App::host_snapshots`, which meant "New Workspace" appeared on a
    /// connected machine **only in the window that had personally connected to
    /// it** — a machine brought up by the reconnect supervisor (app restart, a
    /// dropped link, another window's connect) reached `Link::Connected` with no
    /// home recorded anywhere this window could see, and the row silently
    /// vanished.
    homes: HashMap<HostId, PathBuf>,
}

impl Global for HostLinks {}

impl HostLinks {
    /// The connection to `id`, if this process has one.
    pub fn get(cx: &mut App, id: HostId) -> Option<Arc<RemoteHost>> {
        cx.default_global::<HostLinks>().hosts.get(&id).cloned()
    }

    /// Where a *new* workspace on `id` would start: that machine's own `$HOME`,
    /// never this client's.
    pub fn home(cx: &mut App, id: HostId) -> Option<PathBuf> {
        cx.default_global::<HostLinks>().homes.get(&id).cloned()
    }

    /// Record a connection, and register the same object with the host registry
    /// so the file tree / git / editor reach it the way they reach any host.
    ///
    /// `home` rides along rather than sitting behind its own setter so that no
    /// connect path can register a machine and forget to say where its `$HOME`
    /// is — which is exactly how the reconnect path lost it.
    pub fn insert(cx: &mut App, host: Arc<RemoteHost>, home: PathBuf) {
        let id = host.id();
        crate::ui::host_registry::HostRegistry::insert(cx, Arc::clone(&host).into_shared());
        let table = cx.default_global::<HostLinks>();
        table.hosts.insert(id, host);
        table.homes.insert(id, home);
    }

    /// Drop a machine's connection once nothing is using it.
    pub fn remove(cx: &mut App, id: HostId) {
        let table = cx.default_global::<HostLinks>();
        table.hosts.remove(&id);
        table.homes.remove(&id);
        crate::ui::host_registry::HostRegistry::remove(cx, id);
    }

    /// Machines currently connected. Diagnostics and teardown.
    pub fn len(cx: &mut App) -> usize {
        cx.default_global::<HostLinks>().hosts.len()
    }
}

// ---------------------------------------------------------------------------
// 6. Install consent
// ---------------------------------------------------------------------------

/// The prompt shown before tty7 writes a binary onto someone else's machine.
///
/// Every field of [`InstallRequest`] appears, because the point of asking is
/// that the user can actually judge the answer: *what* is being written, *where*
/// it lands, *how big* it is, *where it came from*, and the checksum they can
/// verify by hand. A prompt that said only "install the server?" would be a
/// consent ritual rather than consent.
pub fn install_detail(request: &InstallRequest) -> String {
    format!(
        "tty7 will write its server binary to {machine} so this machine can host \
         workspaces there. Nothing else on {machine} is touched, and no sudo is used.\n\
         \n\
         Path\u{2003}{path}\n\
         Version\u{2003}{version} ({asset})\n\
         Size\u{2003}{size}\n\
         From\u{2003}{url}\n\
         SHA-256\u{2003}{sha}\n\
         \n\
         Later upgrades on this machine install silently.",
        machine = request.host,
        path = request.remote_path,
        version = request.version,
        asset = request.asset,
        size = human_bytes(request.size_bytes),
        url = request.source_url,
        sha = request.sha256,
    )
}

/// The prompt's title. Names the machine, because a user with several open
/// windows needs to know which one is asking.
pub fn install_title(request: &InstallRequest) -> String {
    format!("Install tty7's server on \u{201c}{}\u{201d}?", request.host)
}

/// Bytes as the user thinks of them. Binary units with one decimal, matching the
/// download sizes shown elsewhere in the app.
///
/// Shared with the switcher's install bar so the size quoted in the consent
/// prompt and the size counting up underneath it are formatted identically —
/// they are the same number, and "8.2 MiB" beside "8.2 MB" would look like two.
pub fn human_bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    let n = n as f64;
    if n < KIB {
        return format!("{} bytes", n as u64);
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

/// How long a blocked installer waits for the user before giving up.
///
/// Generous — a prompt can sit behind another window — but finite, because the
/// thread parked on it is holding an SSH connect open. Timing out **declines**:
/// that is the same answer [`crate::daemon::install::DenyInstall`] gives, and an
/// unanswered question is not consent.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(180);

/// One install waiting on an answer.
pub struct PendingInstall {
    pub request: InstallRequest,
    reply: std::sync::mpsc::SyncSender<InstallDecision>,
}

impl PendingInstall {
    /// Answer it. Dropping a `PendingInstall` without answering leaves the
    /// installer to time out and decline, which is the safe direction.
    pub fn answer(self, decision: InstallDecision) {
        let _ = self.reply.send(decision);
    }
}

static MAILBOX: Mutex<Vec<PendingInstall>> = Mutex::new(Vec::new());

/// The consent handler the GUI registers at startup ([`register`]).
///
/// `confirm` is called on whichever thread is doing the install, which is never
/// the UI thread, so it parks the request in [`MAILBOX`] and blocks. The GUI
/// picks it up with [`take_pending_install`] while a connect is in flight and
/// answers it.
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

/// The latest progress report, per machine.
///
/// Keyed by [`HostId`] rather than by the label the user typed, because the
/// string the installer reports is a *daemon-side* connection key
/// (`install::connection_label`) — the same one a relayed mismatch carries, and
/// the same one [`origin_host`] exists to translate. An alias like `java` never
/// reaches that side.
///
/// Several machines can be installing at once (two windows, two connects), so
/// this is a map and not a slot. [`clear_install_progress`] drops an entry as
/// soon as its connect settles.
static PROGRESS: Mutex<Vec<(HostId, InstallPhase)>> = Mutex::new(Vec::new());

/// The progress sink the GUI registers ([`register`]).
///
/// Called from whichever thread is moving bytes — the routed connection's
/// reader, in the normal case — so it does nothing but overwrite the machine's
/// slot. The panel picks it up on the poll it already runs while a connect is in
/// flight (`watch_for_install_consent`), which is what keeps a burst of reports
/// from becoming a burst of repaints.
pub struct GuiInstallProgress;

impl InstallProgress for GuiInstallProgress {
    fn report(&self, host: &str, phase: InstallPhase) {
        // Same fallback as the auth relay's: a key this client never noted an
        // origin for still resolves to a stable id, so an install is never
        // silently unattributable.
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

/// What `host` last reported, if it is installing right now.
pub fn install_progress_for(host: HostId) -> Option<InstallPhase> {
    let slots = PROGRESS.lock().ok()?;
    slots
        .iter()
        .find(|(known, _)| *known == host)
        .map(|(_, phase)| *phase)
}

/// Forget a machine's progress. Called when a connect settles either way: on
/// success the install is over, and on failure the error takes the same space
/// the bar was using.
pub fn clear_install_progress(host: HostId) {
    if let Ok(mut slots) = PROGRESS.lock() {
        slots.retain(|(known, _)| *known != host);
    }
}

/// Install the GUI's consent handler. Called once at startup; without it the
/// process-wide default declines every install, which is deliberate — a tty7
/// with no UI attached must not decide on the user's behalf that writing to
/// their servers is fine.
pub fn register(cx: &mut App) {
    crate::daemon::install::set_install_confirm(Arc::new(GuiInstallConfirm));
    crate::daemon::install::set_install_progress(Arc::new(GuiInstallProgress));
    crate::daemon::router::set_route_auth_responder(Arc::new(GuiRouteAuth));
    // Touch the globals so the first connect isn't also the first allocation of
    // the table it writes into, on a thread that is holding a socket open.
    let _ = HostLinks::len(cx);
}

/// The oldest install waiting for an answer, if any.
pub fn take_pending_install() -> Option<PendingInstall> {
    MAILBOX.lock().ok()?.pop()
}

// ---------------------------------------------------------------------------
// 6b. Auth prompts on a routed connection
// ---------------------------------------------------------------------------

/// One interactive SSH question waiting on an answer.
///
/// The same mailbox shape as [`PendingInstall`], and for the same reason: the
/// question is raised on a background thread holding a connect open, and only
/// the UI thread can put a sheet in front of a person.
///
/// **Why a routed connection needs its own mailbox at all.** A native-SSH
/// *pane*'s prompts ride that pane's own stream and land in its
/// `TerminalView` — `RemoteTerminal::take_auth_prompt`. A remote workspace's
/// connect has no pane and no view yet; the prompt arrives during the route
/// setup, before anything exists to render it.
pub struct PendingAuth {
    /// Which machine is asking.
    ///
    /// Without it the queue in `ui::remote_workspace` could only be a global
    /// "one at a time" latch: a prompt would have no owner, so a sheet could
    /// not say which box wants the password, and two machines asking at once
    /// could not be told apart. Resolved from the target on the connection's own
    /// [`RouteHeader`] (see [`RouteOrigins`]).
    pub host: HostId,
    pub prompt: AuthPromptKind,
    reply: std::sync::mpsc::SyncSender<AuthResponse>,
}

impl PendingAuth {
    /// Answer it. Dropping one unanswered lets the connect time out and cancel,
    /// which fails the auth step cleanly rather than hanging.
    pub fn answer(self, response: AuthResponse) {
        let _ = self.reply.send(response);
    }
}

static AUTH_MAILBOX: Mutex<Vec<PendingAuth>> = Mutex::new(Vec::new());

/// One machine, under both names it has: the router's
/// ([`RouteTarget::origin_key`]) and this client's ([`HostId`] /
/// [`RemoteTarget`]).
struct RouteOrigin {
    key: String,
    target: RemoteTarget,
    host: HostId,
}

/// Every machine this client has written a [`RouteHeader`] for.
///
/// **Why a table and not the connecting thread.** A question raised while a
/// routed connection is being set up has to be attributed to a machine: the
/// sheet names it, the start-up queue is keyed by it (D7), and
/// `raise_auth_sheet` finds a window with it. That used to be read off a
/// thread-local set by [`connect_blocking`], which held for the workspace
/// connect and quietly did not for a pane's — `connect_routed` lives in
/// `terminal::` and cannot reach `ui::`, so it set nothing and every routed pane
/// prompt was attributed to no machine at all.
///
/// The router names the machine on the header instead, and this maps that name
/// back to the id this client files it under. Both directions are needed
/// because neither side can compute the other's: the daemon has no idea a
/// machine is "the `build` alias" or "profile 7f3…", and the client cannot
/// re-derive a saved profile from the endpoint the daemon knows it by.
///
/// A plain static rather than a gpui `Global`: it is read on whichever
/// background thread is holding the connect open, which is never the UI thread.
/// Entries are never removed — one per machine addressed in a session, and a
/// machine's identity does not stop being true.
static ORIGINS: Mutex<Vec<RouteOrigin>> = Mutex::new(Vec::new());

/// Record that `target` is the machine the router calls `route.origin_key()`.
///
/// Called from every place this client turns a [`RemoteTarget`] into a route
/// header, which is the only moment both names are in hand at once.
pub fn note_origin(route: &crate::daemon::router::RouteTarget, target: &RemoteTarget) {
    let key = route.origin_key();
    let Ok(mut origins) = ORIGINS.lock() else {
        return;
    };
    let host = target.host_id();
    match origins.iter_mut().find(|o| o.key == key) {
        // Last writer wins. Two saved profiles can differ only in credentials
        // and so share an endpoint — and therefore a key — in which case
        // "whichever the user most recently addressed" is the best available
        // answer to which of them a prompt is for. It is the *machine* that is
        // certain here, and the machine is what the sheet and the queue need.
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

/// This client's id for the machine the router names `key`, if it has one.
pub fn origin_host(key: &str) -> Option<HostId> {
    let origins = ORIGINS.lock().ok()?;
    origins.iter().find(|o| o.key == key).map(|o| o.host)
}

/// This client's target for the machine the router names `key`.
///
/// The mismatch prompt's route back: [`MismatchedRemoteDaemon::host`] is a
/// daemon-side connection label, which is the same string
/// [`RouteTarget::origin_key`](crate::daemon::router::RouteTarget::origin_key)
/// produces for an SSH machine — so this is how "restart the server on *that*
/// box" turns back into a header this client can write.
pub fn origin_target(key: &str) -> Option<RemoteTarget> {
    let origins = ORIGINS.lock().ok()?;
    origins
        .iter()
        .find(|o| o.key == key)
        .map(|o| o.target.clone())
}

/// The routed-auth handler the GUI registers at startup ([`register`]).
pub struct GuiRouteAuth;

impl crate::daemon::router::RouteAuthResponder for GuiRouteAuth {
    fn respond(
        &self,
        machine: &crate::daemon::router::RouteTarget,
        prompt: &AuthPromptKind,
    ) -> AuthResponse {
        let key = machine.origin_key();
        // A machine this client has never written a header for cannot raise a
        // prompt — every routed connection starts with one. If one ever does,
        // it is attributed to an id derived from the router's own name for it
        // rather than dropped: an entry under a key no window matches still gets
        // answered (and times out cleanly); a lost one hangs a connect.
        let host = origin_host(&key).unwrap_or_else(|| HostId::from_connection_key(&key));
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        {
            let Ok(mut mailbox) = AUTH_MAILBOX.lock() else {
                return AuthResponse::Cancelled;
            };
            mailbox.push(PendingAuth {
                host,
                prompt: prompt.clone(),
                reply: tx,
            });
        }
        rx.recv_timeout(CONSENT_TIMEOUT)
            .unwrap_or(AuthResponse::Cancelled)
    }
}

/// The oldest routed auth prompt waiting for an answer, if any.
///
/// Polled by the connect watcher beside [`take_pending_install`]. While nothing
/// polls it the prompts time out and cancel — byte-for-byte the behaviour this
/// path had before the relay, so wiring the sheet is an improvement and never a
/// regression.
pub fn take_pending_auth() -> Option<PendingAuth> {
    AUTH_MAILBOX.lock().ok()?.pop()
}

/// Whose turn it is to drain [`AUTH_MAILBOX`], for tests only.
///
/// The mailbox is process-global, and [`pump_auth_sheets`] takes *every* entry in
/// one pass — correct for the app, where one tick serves one mailbox, and fatal
/// in a test binary, where a test waiting for the prompt it just caused shares
/// that mailbox with every gpui test driving a tick. The prompt gets drained by a
/// tick that has no idea it was spoken for, and the waiting test never sees it.
///
/// So a test that needs its own prompt back claims this first, and the drain
/// yields while it is held. Compiled out of a release build, where there is one
/// app, one tick and nothing to arbitrate.
///
/// [`pump_auth_sheets`]: crate::ui::remote_workspace::pump_auth_sheets
#[cfg(test)]
pub(crate) static MAILBOX_TURN: Mutex<()> = Mutex::new(());

/// Claim [`MAILBOX_TURN`], ignoring poisoning: a test that panicked while holding
/// it has nothing to corrupt here — the guard protects an ordering, not data.
#[cfg(test)]
pub(crate) fn claim_mailbox() -> std::sync::MutexGuard<'static, ()> {
    MAILBOX_TURN.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// 7. Remote daemon version skew
// ---------------------------------------------------------------------------

/// The answers the dialect-mismatch prompt offers, in the order `window.prompt`
/// takes them — **index 1 is the destructive one**, which is what
/// `prompt_remote_daemon_mismatch` matches on.
///
/// Written down here rather than at the prompt because [`mismatch_detail`] spells
/// both out by name in its body: a detail explaining a button that is no longer
/// there is worse than no explanation at all. `Keep Sessions` used to be index 0
/// and had to go, which is precisely the drift this prevents repeating.
pub const MISMATCH_ANSWERS: [&str; 2] = ["Cancel", "Restart Server"];

/// The restart-or-cancel question for a remote `tty7-server` this client cannot
/// talk to.
///
/// **There is no "keep and carry on" here, and the wording must not imply one.**
/// A mismatch is only ever recorded when the running daemon's *dialects* are not
/// ours (`Installer::check_running_build`) — a merely different build that can
/// still speak to us is reused in silence and never reaches this prompt. The
/// workspace connects to the daemon that is running, so leaving it in place
/// means the connection fails in the handshake. The real choice is between
/// ending that machine's sessions and not connecting at all, and saying so is
/// the difference between a decision and a trick.
pub fn mismatch_detail(m: &MismatchedRemoteDaemon) -> String {
    let running = match (&m.running_version, &m.running_exe) {
        (Some(v), Some(exe)) => format!("{v} (from {exe})"),
        (Some(v), None) => v.clone(),
        (None, Some(exe)) => format!("an unknown build (from {exe})"),
        (None, None) => "an unknown build".to_string(),
    };
    format!(
        "{host} is serving tty7 sessions from {running}, which speaks a protocol \
         this client ({wanted}) cannot. tty7 has installed a matching server there, \
         but the one already running is the one your sessions are on.\n\
         \n\
         Restart Server\u{2003}starts {wanted} there and ends every session it is hosting.\n\
         Cancel\u{2003}leaves {host} exactly as it is. This window will not connect.",
        host = m.host,
        wanted = m.wanted_version,
    )
}

/// The prompt's title.
pub fn mismatch_title(m: &MismatchedRemoteDaemon) -> String {
    format!("Restart tty7's server on \u{201c}{}\u{201d}?", m.host)
}

/// The machine a mismatch record is about, as this client knows it.
///
/// `None` when the record names a machine no header in this session addressed —
/// which cannot happen for a mismatch (it is discovered *while* opening a routed
/// connection this client asked for), and which the caller turns into a refusal
/// rather than a guess.
pub fn mismatch_target(m: &MismatchedRemoteDaemon) -> Option<RemoteTarget> {
    origin_target(&m.host)
}

/// Carry out "Restart Server": stop the `tty7-server` on the
/// machine `header` names and start this client's build. **Blocking**, and
/// **every pane that server hosts dies** — only ever call this with the user's
/// explicit answer behind it.
///
/// The connection is a setup window and nothing else: the daemon acks and both
/// ends close. Reconnecting afterwards is the supervisor's job, not this one's.
pub fn restart_server_blocking(header: RouteHeader, label: &str) -> Result<(), String> {
    let action = header.action;
    crate::daemon::spawn::ensure_running()
        .map_err(|e| format!("tty7's local daemon could not be started: {e}"))?;
    let mut stream = crate::daemon::transport::connect()
        .map_err(|e| format!("could not reach tty7's local daemon: {e}"))?;
    let ack = crate::daemon::router::negotiate(&mut stream, &header)
        .map_err(|e| format!("could not restart tty7's server on {label}: {e}"))?;
    // An older local daemon does not know the action and forwards the
    // connection instead — a link, not a restart. Saying nothing happened is the
    // only honest answer; the alternative is a "done" over a server still
    // running the old build.
    if !ack.performed(action) {
        return Err(format!(
            "this machine's tty7 daemon is an older build and cannot restart the server on \
             {label}. Quit tty7 (which stops the daemon) and open it again, then retry."
        ));
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
            asset: "tty7-server-x86_64-unknown-linux-musl",
            source_url: "https://example.invalid/v0.9.1/tty7-server".into(),
            remote_path: "/home/me/.local/share/tty7/bin/tty7-server-0.9.1".into(),
            size_bytes: 9_437_184,
            sha256: "abc123".into(),
        }
    }

    /// The confirmation says *what* is written, *where*,
    /// *how big* and *where from*. A field silently dropped from the prompt
    /// would turn an informed decision back into a blind one, so every one of
    /// them is pinned here rather than eyeballed.
    #[test]
    fn the_install_prompt_states_every_field_of_the_request() {
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
        // The size is shown in units, not raw bytes.
        assert!(detail.contains("9.0 MiB"), "{detail}");
        // And the title names the machine, so a user with several windows open
        // knows which one is asking.
        assert!(install_title(&request).contains("me@build-box:22"));
    }

    #[test]
    fn human_bytes_reads_in_binary_units() {
        assert_eq!(human_bytes(512), "512 bytes");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1_572_864), "1.5 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    /// An unanswered consent request must not read as approval — the default
    /// everywhere in the install path is to decline, and a dropped prompt is
    /// exactly the case where nobody said yes.
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

    /// The handler parks the request rather than deciding, so the GUI can pick
    /// it up; and the mailbox hands it back exactly once.
    #[test]
    fn the_gui_handler_parks_the_request_for_the_ui_to_answer() {
        // Drain anything a sibling test left behind — the mailbox is process-wide.
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

    /// **A routed auth prompt knows which machine asked.**
    ///
    /// The attribution rides the connection's own [`RouteHeader`] — the router
    /// names the machine, [`RouteOrigins`] maps that name back to this client's
    /// id. Without the host the start-up queue could only be a global "one at a
    /// time" latch: no sheet could name the box, and two machines asking at once
    /// could not be told apart.
    ///
    /// Answered **on another thread than the one that noted the origin**, which
    /// is the point: the mechanism this replaced could only work when the two
    /// were the same thread, and on the pane path they never are.
    #[test]
    fn a_routed_auth_prompt_carries_the_machine_that_raised_it() {
        // Held for the whole exchange: the prompt this test is about to cause
        // goes into a process-global mailbox, and `pump_auth_sheets` drains all of
        // it from any gpui test in this binary that drives a tick. Without the
        // claim that drain takes this test's prompt and the wait below never ends.
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
        // Bounded, because this loop is the difference between a stolen prompt
        // being a failure and being a *hang*. `AUTH_MAILBOX` is process-global
        // and `pump_auth_sheets` drains every entry in one pass, so any gpui test
        // in this binary that drives a tick can take this prompt before the line
        // below does — and unbounded, this test then spins until CI's six-hour
        // job limit. It has: a `main` run sat inside this test for 2h50m, and
        // three Windows runs before it went the same way, none of them naming a
        // test until the run was cancelled and its partial log read back.
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

    /// **Both routed paths land on the same machine.** A workspace's control
    /// connection and one of its panes are built from the same SSH spec but by
    /// different code on different threads (`connect_blocking` and
    /// `connect_routed`), and a prompt raised on either has to name the one
    /// machine — the queue, the sheet and the window lookup are all keyed by it.
    ///
    /// The pane header differs from the control one (`--pane`, `channel: Pane`)
    /// and must *not* look like a second machine, which is what the shared
    /// origin key buys.
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

        // And a `--stdio` machine's two headers agree too, though the pane's
        // argv carries `--pane` and the control one does not.
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

    /// The mismatch prompt's way home: the daemon labels a mismatch with the
    /// connection key, which is the same string the router names the machine
    /// by — so "restart the server on *that* box" resolves to a target this
    /// client can build a header from. Without it the restart has nowhere to go.
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

        // A machine this client never addressed answers `None` rather than a
        // guess — the restart refuses instead of acting on the wrong box.
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
        assert!(mismatch_title(&m).contains("me@build-box:22"));

        // A daemon whose build could not be read still produces a usable prompt.
        let unknown = MismatchedRemoteDaemon {
            running_version: None,
            running_exe: None,
            ..m
        };
        assert!(mismatch_detail(&unknown).contains("an unknown build"));
    }

    /// The detail explains the buttons by name, so it has to name the ones that
    /// are actually there. This is a prompt whose whole job is to make a
    /// destructive choice legible; a body describing an answer the prompt does
    /// not offer (as it did while `Keep Sessions` was one of them) turns that
    /// back into a guess.
    #[test]
    fn the_mismatch_detail_explains_every_answer_the_prompt_offers() {
        let detail = mismatch_detail(&MismatchedRemoteDaemon {
            host: "me@build-box:22".into(),
            running_version: Some("0.8.0".into()),
            running_exe: None,
            wanted_version: "0.9.1".into(),
        });
        for answer in MISMATCH_ANSWERS {
            assert!(detail.contains(answer), "{answer} is unexplained: {detail}");
        }
    }

    #[test]
    fn endpoint_labels_hide_the_default_port() {
        assert_eq!(endpoint_label("me", "box.local", 22), "me@box.local");
        assert_eq!(endpoint_label("me", "box.local", 2222), "me@box.local:2222");
        assert_eq!(endpoint_label("", "box.local", 22), "box.local");
    }

    /// The picker's rows come from the machine's tree: newest first, with a
    /// name derived the way a local workspace's would be when none is set.
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

    /// An empty query is not a search: it must hand back the list exactly as
    /// [`available_hosts`] built it (profiles first, aliases after), because
    /// that order carries information a score cannot reproduce.
    #[test]
    fn an_empty_query_keeps_every_machine_in_order() {
        let hosts = vec![
            host("gate2jup", "root@18.143.92.244"),
            host("aws-xy", "root@52.199.113.213"),
        ];
        let all = filter_hosts(&hosts, "   ");
        assert_eq!(all, hosts);
    }

    /// The two things a user types: the name they gave the box, and the address
    /// they remember it by. Both find it; a name match outranks an address one.
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

        // "or" is in `orb`'s name and in the other two's `root@` — the machine
        // actually called that comes first.
        let mixed = filter_hosts(&hosts, "or");
        assert_eq!(mixed[0].label, "orb", "a name match beats an endpoint one");
    }

    /// A query nothing answers filters everything out rather than falling back
    /// to the full list — the panel's empty state says so, and a silent "here
    /// is everything" would read as the search being broken.
    #[test]
    fn a_query_nothing_matches_returns_nothing() {
        let hosts = vec![host("gate2jup", "root@18.143.92.244")];
        assert!(filter_hosts(&hosts, "zzz").is_empty());
    }

    /// A distro row carries the exact string `wsl -d` takes, because that is
    /// what [`RouteHeader::wsl`](crate::daemon::router::RouteHeader::wsl) will
    /// be handed and what `wsl:<distro>` keys the machine by. A row whose label
    /// were prettied up ("Ubuntu 22.04") would connect to nothing.
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

    /// Nothing installed is not an empty *section* — it is no section at all,
    /// which is what keeps the band off a Mac and off a Windows box with no WSL.
    #[test]
    fn no_distros_is_no_rows() {
        assert!(wsl_choices(&[]).is_empty());
    }

    /// Both ways a user looks for a distro: by its name, and by the kind of
    /// thing it is. The second only works because the endpoint column says
    /// `WSL`, and [`host_score`] searches it.
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

    /// **A probe that could not answer is not an empty machine list.** `wsl.exe`
    /// refuses while a `wsl --shutdown` is in flight; adopting that as "you have
    /// no distributions" would empty the switcher for a TTL over a command that
    /// changed nothing.
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

    /// An answer is adopted whole, *including* an empty one — unregistering the
    /// last distribution has to take its row away, or the picker offers a machine
    /// that cannot be reached.
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

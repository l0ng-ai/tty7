//! Session persistence: remember the tab / split-pane layout and each
//! terminal's working directory across restarts, plus a stack of recently
//! closed tabs for "Reopen Closed Tab".
//!
//! The on-disk model mirrors the live `Pane` tree but stays purely
//! serializable (no GPUI entities, no `gpui::Axis` which isn't `Serialize`).
//! It lives at `~/.config/tty7/session.json`, alongside `config.json`.
//!
//! All IO and parsing is best-effort: a missing/corrupt file just means "no
//! session to restore", and write failures are logged rather than fatal — the
//! app must never crash or stall over session bookkeeping.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::daemon::protocol::NativeSshSpec;

/// Split orientation, mirroring `gpui::Axis` (which isn't `Serialize`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SessionAxis {
    Horizontal,
    Vertical,
}

/// A serializable mirror of one tab's `Pane` tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionPane {
    /// A single terminal, restored in `cwd` (or the default dir if `None`).
    Leaf {
        #[serde(default)]
        cwd: Option<PathBuf>,
        /// Daemon pane id this leaf was mirroring. On restore we re-`attach` to
        /// it when the daemon still has it alive (process + scrollback intact),
        /// else fall back to spawning a fresh shell in `cwd`. `None` for sessions
        /// written by an older build (they just spawn fresh).
        #[serde(default)]
        pane_id: Option<u64>,
        /// The native-SSH spec this leaf ran, **with secrets stripped**
        /// ([`NativeSshSpec::without_secrets`]). Persisted so a *dead* native-SSH
        /// pane can be respawned (reconnected) on restore rather than falling back
        /// to a local shell — the reconnection UX itself is WS6's. A live pane
        /// reattaches for free and needs none of this. `None` for local panes and
        /// for sessions written before this field existed.
        #[serde(default)]
        ssh_spec: Option<Box<NativeSshSpec>>,
        /// The coding agent this leaf was running at save time, plus its native
        /// session id (from the agent's own `session-start` event). When the
        /// pane can't re-attach on restore, these drive the cmux-style resume:
        /// the fresh shell is handed the agent's resume command
        /// (`claude --resume <id>`, …) so the conversation continues. `None`
        /// for panes without an agent, agents without hooks, or old sessions.
        #[serde(default)]
        agent: Option<crate::core::cli_agent::CLIAgent>,
        #[serde(default)]
        agent_session_id: Option<String>,
        /// The argv the agent was launched with, as the daemon observed it —
        /// lets the resume command carry the user's launch flags
        /// (`--dangerously-skip-permissions`, …) instead of resuming bare.
        /// `None` for old sessions or when nothing was captured.
        #[serde(default)]
        agent_launch_argv: Option<Vec<String>>,
    },
    /// A split of two subtrees along `axis`, with `a` taking `ratio` of space.
    Split {
        axis: SessionAxis,
        #[serde(default = "default_ratio")]
        ratio: f32,
        a: Box<SessionPane>,
        b: Box<SessionPane>,
    },
}

fn default_ratio() -> f32 {
    0.5
}

/// A serializable mirror of one tab: its pane tree plus an optional user-set
/// name (from "Rename Tab"). A missing `name` falls back to the title-derived
/// label at render time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTab {
    #[serde(default)]
    pub name: Option<String>,
    pub pane: SessionPane,
    /// The tab's last-known sidebar repo group (its repository home — the
    /// main checkout's root, shared by all its linked worktrees), so a
    /// restored session renders grouped immediately instead of starting flat
    /// and reshuffling as git probes land. `None` = Scratch / never resolved.
    ///
    /// **A bare path, and that is sound.** A path alone cannot say *which*
    /// machine it is on, and [`HostId`](crate::host::HostId) — which could —
    /// is deliberately not persistable. The qualifier is not missing, it is
    /// factored out: a tab always belongs to exactly one [`Workspace`], a
    /// workspace names exactly one machine in [`Workspace::host`], and a
    /// window shows exactly one workspace — mixing local and remote tabs in one
    /// window is the thing tty7 never does. So the fully-qualified group key
    /// is `(workspace.host_id(), tab.sidebar_group)`, with the host half
    /// stored once per workspace instead of once per tab. Two machines whose
    /// repos share a root path can only collide inside one window, which the
    /// model does not permit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_group: Option<std::path::PathBuf>,
}

/// One workspace's contents: the open tabs and which one was active.
///
/// This is the unit a single window displays. It used to *be* the whole file
/// (tty7 had exactly one window); it is now nested inside a [`Workspace`], and
/// [`Workspaces`] owns the file-level IO.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub active: usize,
    pub tabs: Vec<SessionTab>,
}

/// Stable identity for a workspace, minted once when it is first created and
/// carried across restarts. Windows are transient views; *this* is what the
/// workspace picker reopens and what a window handle maps back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(uuid::Uuid);

impl WorkspaceId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }

    /// A stable numeric key for gpui element ids, which need something
    /// hashable and cheap rather than a freshly formatted string each frame.
    pub fn element_key(&self) -> u64 {
        self.0.as_u64_pair().0
    }
}

impl Default for WorkspaceId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

// ---------------------------------------------------------------------------
// Remote references
// ---------------------------------------------------------------------------

/// The machine a remote workspace lives on, named the way the user already
/// named it.
///
/// **This is a pointer, never a configuration.** It is a hard rule that a
/// machine is configured once and that remote workspaces reuse what is already
/// there — the profile's keys, its jump host, its `ProxyCommand` — so this type
/// has exactly one job: say *which* existing entry to connect through. The
/// three variants are the three places an SSH target can already have been
/// spelled out in tty7 today.
///
/// | Variant | Where it came from | Connection key |
/// |---|---|---|
/// | [`Profile`](RemoteTarget::Profile) | A saved [`SshProfile`](crate::core::ssh_profile::SshProfile), by its stable uuid | `ssh-profile:<uuid>` |
/// | [`Alias`](RemoteTarget::Alias) | A `Host` stanza in `~/.ssh/config` | `ssh-alias:<alias>` |
/// | [`Direct`](RemoteTarget::Direct) | A typed `user@host:port` (QuickConnect) | `ssh-direct:<user>@<host>:<port>` |
/// | [`Wsl`](RemoteTarget::Wsl) | A WSL distro — **M8**, defined only so the key table has no hole | `wsl:<distro>` |
///
/// Persisted, unlike [`HostId`](crate::host::HostId): this is what survives a
/// restart, and the id is derived from it at connect time.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteTarget {
    /// A saved SSH profile, referenced by [`SshProfile::id`](crate::core::ssh_profile::SshProfile::id).
    Profile { id: uuid::Uuid },
    /// A `Host` alias from `~/.ssh/config`. Kept verbatim — OpenSSH matches
    /// alias names case-sensitively, so folding case here would point at a
    /// different stanza than `ssh <alias>` would.
    Alias { alias: String },
    /// A target typed straight in, as `parse_quick_connect` understands it.
    Direct {
        /// The login user. Empty means "whatever this client's SSH would use",
        /// which is a *different* connection key than a spelled-out user — see
        /// [`RemoteTarget::connection_key`].
        #[serde(default)]
        user: String,
        /// Hostname or IP, lowercased (DNS is case-insensitive).
        host: String,
        #[serde(default = "default_ssh_port")]
        port: u16,
    },
    /// A WSL distribution. **M8 owns the behaviour**; the variant exists now so
    /// that [`connection_key`](RemoteTarget::connection_key) is a total function
    /// over the table rather than one that grows a case later.
    Wsl { distro: String },
    /// A `tty7-server --stdio` child process on *this* machine — the workspace
    /// mirror of [`RouteTarget::LocalStdio`](crate::daemon::router::RouteTarget::LocalStdio),
    /// and the only way to exercise a real remote workspace end to end without
    /// an sshd.
    ///
    /// **Never offered by the picker.** It is reachable only when
    /// `TTY7_LOCAL_STDIO_SERVER` names a server binary, which is how the
    /// end-to-end tests and a developer's `dev-verify` run stand a machine up.
    /// It grants no authority the socket did not already have: a pane's
    /// `ClientMsg::Spawn` already runs an arbitrary program as this user over
    /// that same user-private socket.
    LocalStdio { program: String, args: Vec<String> },
}

fn default_ssh_port() -> u16 {
    22
}

impl RemoteTarget {
    /// A `user@host:port` target, normalized.
    ///
    /// The host is lowercased here *and* in [`connection_key`](Self::connection_key)
    /// — here so two equal targets compare equal, there so a hand-edited
    /// `session.json` with `Box.Local` still derives the same id as `box.local`.
    pub fn direct(user: impl Into<String>, host: impl Into<String>, port: u16) -> RemoteTarget {
        RemoteTarget::Direct {
            user: user.into(),
            host: host.into().to_ascii_lowercase(),
            port,
        }
    }

    /// Parse `[ssh://]user@host[:port]` into a [`Direct`](RemoteTarget::Direct)
    /// target.
    ///
    /// Deliberately delegates to
    /// [`parse_quick_connect`](crate::core::ssh_profile::parse_quick_connect)
    /// rather than parsing again: "the same string the connection manager
    /// already accepts" is the whole promise of this variant, and a second
    /// parser would be a second opinion about IPv6 brackets and `@` in
    /// usernames. `None` for anything that parser rejects.
    pub fn parse_direct(input: &str) -> Option<RemoteTarget> {
        let q = crate::core::ssh_profile::parse_quick_connect(input)?;
        let port = q.port_or_default();
        Some(RemoteTarget::direct(
            q.user.unwrap_or_default(),
            q.host,
            port,
        ))
    }

    /// The canonical connection string this target hashes to.
    ///
    /// **Contains no workspace id.** Several workspaces on one box share a key,
    /// and therefore share a [`HostId`](crate::host::HostId) and the one SSH
    /// connection underneath it — the granularity the whole design assumes.
    ///
    /// One conservative case worth knowing: `me@box` and a bare `box` are
    /// different keys even when the client's SSH would resolve them to the same
    /// login. That costs a second connection, never a wrong one; merging them
    /// would require resolving `~/.ssh/config` here, and getting *that* wrong
    /// would point two machines at one cache.
    pub fn connection_key(&self) -> String {
        match self {
            RemoteTarget::Profile { id } => format!("ssh-profile:{id}"),
            RemoteTarget::Alias { alias } => format!("ssh-alias:{alias}"),
            RemoteTarget::Direct { user, host, port } => {
                format!("ssh-direct:{user}@{}:{port}", host.to_ascii_lowercase())
            }
            RemoteTarget::Wsl { distro } => format!("wsl:{distro}"),
            RemoteTarget::LocalStdio { program, args } => {
                format!("local-stdio:{program} {}", args.join(" "))
            }
        }
    }

    /// The in-process id this target resolves to.
    ///
    /// This is the **only** bridge between the persisted world and the runtime
    /// one: `RemoteRef` is what survives a restart, `HostId` is what the
    /// in-memory tables key on, and this function is how you get from the first
    /// to the second. There is deliberately no inverse — an id is a hash, and a
    /// structure that wanted to persist "which host" must persist a
    /// [`RemoteTarget`].
    pub fn host_id(&self) -> crate::host::HostId {
        crate::host::HostId::from_connection_key(&self.connection_key())
    }
}

impl std::fmt::Display for RemoteTarget {
    /// A label for a status bar or a picker row. A profile shows as its uuid
    /// because the name lives in the profile store, which this type
    /// deliberately does not reach into.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoteTarget::Profile { id } => write!(f, "{id}"),
            RemoteTarget::Alias { alias } => write!(f, "{alias}"),
            RemoteTarget::Direct { user, host, port } => {
                if !user.is_empty() {
                    write!(f, "{user}@")?;
                }
                write!(f, "{host}")?;
                if *port != 22 {
                    write!(f, ":{port}")?;
                }
                Ok(())
            }
            RemoteTarget::Wsl { distro } => write!(f, "wsl:{distro}"),
            // The path, not the argv: this is a status-bar label, and the
            // arguments are `--stdio` boilerplate that says nothing useful.
            RemoteTarget::LocalStdio { program, .. } => {
                let name = std::path::Path::new(program)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| program.clone());
                write!(f, "local:{name}")
            }
        }
    }
}

/// A workspace that lives on another machine: which machine, and which
/// workspace over there.
///
/// The `workspace` id is the **remote's**, minted once and then used as the key
/// into that machine's `~/.local/share/tty7/workspaces.json`
/// ([`crate::core::workspace_store`]). A client-side [`Workspace`] carrying one
/// of these is a *view*, not the record: its `session` is left empty until the
/// layout is pulled from the remote, which owns it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RemoteRef {
    /// Which machine, in terms of a configuration that already exists.
    pub target: RemoteTarget,
    /// The workspace's id **on that machine**.
    pub workspace: WorkspaceId,
}

impl RemoteRef {
    pub fn new(target: RemoteTarget, workspace: WorkspaceId) -> RemoteRef {
        RemoteRef { target, workspace }
    }

    /// The id of the machine this points at. Two refs to different workspaces
    /// on one box answer the same id.
    pub fn host_id(&self) -> crate::host::HostId {
        self.target.host_id()
    }

    /// The remote store's key for this workspace — what
    /// [`ControlRequest::WorkspaceGet`](crate::daemon::control::ControlRequest::WorkspaceGet)
    /// and friends carry.
    pub fn store_key(&self) -> String {
        self.workspace.to_string()
    }
}

/// A persistent workspace: a named group of tabs that a window can open, close,
/// and reopen later. Closing its window is a *detach* — the panes keep running
/// in the daemon and the entry stays here with `open: false`, which is what the
/// home-page picker lists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    #[serde(default)]
    pub id: WorkspaceId,
    /// User-set name from "Rename Workspace". `None` falls back to
    /// [`Workspace::display_name`], derived from the tabs' repo/cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub session: Session,
    /// Geometry this workspace's window last occupied, so reopening it lands
    /// where the user left it rather than at the shared default. `None` for a
    /// workspace that has never been on screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<crate::core::window_state::WindowState>,
    /// Whether a window was showing this workspace at quit. Launch reopens
    /// exactly the `open` ones; the rest wait in the picker.
    #[serde(default)]
    pub open: bool,
    /// Unix seconds when this workspace was last focused, for "2 minutes ago"
    /// in the picker and for ordering it. 0 == never recorded.
    #[serde(default)]
    pub last_active: u64,
    /// The machine this workspace's panes and files live on. `None` means this
    /// one, **and means it identically to every build that predates the field**:
    /// a `session.json` written before this existed decodes with `None`
    /// throughout, i.e. all-local, which is the behaviour it had.
    ///
    /// A `Some` entry is a *view* of a record that lives over there. Its
    /// `session` is empty until the layout is pulled from the remote's own
    /// store; `window` and `open` stay here, because they are this client's
    /// view state and closing a window at the office must not hide the
    /// workspace from the laptop at home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<RemoteRef>,
    /// Identity of the daemon *process* the pane ids in `session` refer to
    /// (see `daemon::protocol::DaemonVersion::instance`). One field for the
    /// whole workspace, not one per leaf, because a workspace's panes all live
    /// in one daemon (one window, one machine).
    ///
    /// This is what makes a saved pane id safe to trust: daemon ids restart
    /// from 1, so after a reboot every saved id points at whatever unrelated
    /// shell happens to hold the number now — and restore's aliveness check
    /// cannot tell a survivor from a squatter. A claim whose instance differs
    /// from the daemon now serving blanks its ids instead
    /// ([`Workspace::forget_stale_pane_ids`]) and takes the fresh-spawn path,
    /// agent resume included, which is the correct reading of "the daemon
    /// those panes lived in is gone".
    ///
    /// A remote workspace records its machine's `tty7-server` instance here,
    /// for exactly the same reason and read by exactly the same check. The live
    /// per-connection tracking on the client (`note_instance`) does not replace
    /// this: that map is in memory, so it is empty on the launch where it would
    /// matter most — the one after a client restart that spanned a server
    /// replacement.
    ///
    /// `None` for records written before the field, and whenever the serving
    /// process cannot be named (an older peer, a machine not connected). `None`
    /// disables the check, never fails it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_instance: Option<String>,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            id: WorkspaceId::new(),
            name: None,
            session: Session::default(),
            window: None,
            open: true,
            last_active: now_secs(),
            host: None,
            daemon_instance: None,
        }
    }
}

impl Workspace {
    /// Wrap a bare session as a brand-new open workspace.
    pub fn from_session(session: Session) -> Self {
        Self {
            session,
            ..Self::default()
        }
    }

    /// What to show in the picker and the window title: the user-set name if
    /// any, else the repository most of its tabs live in, else the first tab's
    /// directory, else a generic fallback. Derived rather than stored so a
    /// workspace that `cd`s into a project stops being "Untitled" on its own.
    pub fn display_name(&self) -> String {
        if let Some(name) = self
            .name
            .as_ref()
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
        {
            return name.to_string();
        }
        if let Some(repo) = self.dominant_repo() {
            if let Some(base) = basename(&repo) {
                return base;
            }
        }
        if let Some(cwd) = self.first_cwd() {
            if let Some(base) = basename(&cwd) {
                return base;
            }
        }
        "Untitled".to_string()
    }

    /// The repo root the most tabs belong to — the workspace's centre of
    /// gravity for naming. Ties break toward the earliest tab, matching the
    /// order the user sees in the sidebar.
    pub fn dominant_repo(&self) -> Option<PathBuf> {
        let mut counts: Vec<(PathBuf, usize)> = Vec::new();
        for group in self
            .session
            .tabs
            .iter()
            .filter_map(|t| t.sidebar_group.as_ref())
        {
            match counts.iter_mut().find(|(path, _)| path == group) {
                Some((_, n)) => *n += 1,
                None => counts.push((group.clone(), 1)),
            }
        }
        counts
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(path, _)| path)
    }

    /// The first saved cwd anywhere in the tab tree, used for naming and for
    /// the picker's dim subtitle line.
    pub fn first_cwd(&self) -> Option<PathBuf> {
        self.session
            .tabs
            .iter()
            .find_map(|tab| first_leaf_cwd(&tab.pane))
    }

    /// Total leaf terminals across every tab — the picker's "3 panes" count.
    pub fn pane_count(&self) -> usize {
        self.session.tabs.iter().map(|t| leaf_count(&t.pane)).sum()
    }

    /// Every daemon pane id this workspace claims, for the cross-window
    /// uniqueness check on restore (two windows attaching one pane would let
    /// the second silently steal the first's stream).
    pub fn pane_ids(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for tab in &self.session.tabs {
            collect_pane_ids(&tab.pane, &mut out);
        }
        out
    }

    /// Drop every saved pane id, keeping the layout. Answers how many were
    /// dropped, so a caller with nothing to forget can skip the write.
    ///
    /// For the one caller that *knows* the panes are gone: ending a workspace's
    /// sessions kills them and then leaves the record on file to be reopened.
    /// The ids in it are ours to invalidate — we are what killed them — and a
    /// leaf with no id is exactly what restore needs to see, because that is
    /// the path that spawns a fresh shell in the saved cwd and hands a coding
    /// agent its `--resume`. Left in place they are a promise the machine
    /// cannot keep: the reattach finds nothing, and on a remote workspace it
    /// used to have no way to say so.
    pub fn forget_pane_ids(&mut self) -> usize {
        let mut forgotten = 0;
        for tab in &mut self.session.tabs {
            forgotten += blank_pane_ids(&mut tab.pane);
        }
        forgotten
    }

    /// Blank every saved pane id if it was recorded against a *different*
    /// daemon process than `current` — see [`Workspace::daemon_instance`] for
    /// the id-reuse failure this closes. Answers how many ids were dropped.
    ///
    /// Only a **known, differing** instance pair trips it. `None` on either
    /// side means "cannot tell" (an old record, an old daemon), and treating
    /// that as stale would respawn every pane on the first launch after an
    /// upgrade — exactly the sessions persistence exists to keep.
    ///
    /// The agent fields stay, deliberately: unlike a *duplicate* claim (see
    /// `drop_duplicate_pane_ids`), a stale-instance claim means the pane is
    /// genuinely gone with its daemon, nothing else is running the
    /// conversation, and the fresh shell resuming it is the feature.
    pub fn forget_stale_pane_ids(&mut self, current: Option<&str>) -> usize {
        let (Some(recorded), Some(current)) = (self.daemon_instance.as_deref(), current) else {
            return 0;
        };
        if recorded == current {
            return 0;
        }
        self.forget_pane_ids()
    }

    /// Stamp this workspace as just-focused.
    pub fn touch(&mut self) {
        self.last_active = now_secs();
    }

    // ----- the local / remote split ----------------------------------------

    /// A client-side entry for a workspace that lives on another machine.
    ///
    /// The `session` is left empty on purpose: the remote's
    /// `~/.local/share/tty7/workspaces.json` is the authority for the layout,
    /// and it is pulled on connect. Filling it in from a stale local guess would
    /// make the window flash a layout the machine has since moved on from.
    pub fn on_remote(host: RemoteRef) -> Workspace {
        Workspace {
            host: Some(host),
            ..Workspace::default()
        }
    }

    /// Whether this workspace lives on another machine.
    pub fn is_remote(&self) -> bool {
        self.host.is_some()
    }

    /// The id of the machine this workspace's panes are on.
    ///
    /// This is the qualifier that turns a bare path or a bare `pane_id` into
    /// something globally meaningful: `pane_id` is unique only within one remote
    /// server, so the client's pane identity is `(host_id, pane_id)`, and a
    /// repo root is unique only within one machine, so a sidebar group key is
    /// `(host_id, sidebar_group)`. Storing it once per workspace rather than
    /// once per pane is exactly what the one-window-one-machine rule buys.
    pub fn host_id(&self) -> crate::host::HostId {
        match &self.host {
            Some(r) => r.host_id(),
            None => crate::host::HostId::LOCAL,
        }
    }

    /// The record the **remote** owns, as the JSON that crosses the wire in a
    /// [`WorkspacePut`](crate::daemon::control::ControlRequest::WorkspacePut).
    ///
    /// The storage split, executable rather than aspirational: what
    /// stays here is `window`, `open` and `host` — this client's view state —
    /// and what goes over there is everything that is a fact about the machine.
    /// [`REMOTE_OWNED_FIELDS`] pins the split, and a test fails if a new field
    /// is added without a decision about which side it belongs to.
    pub fn to_remote_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let Some(obj) = value.as_object_mut() {
            obj.retain(|k, _| REMOTE_OWNED_FIELDS.contains(&k.as_str()));
        }
        value
    }

    /// Merge an authoritative record pulled from a remote store into this entry.
    ///
    /// Touches only the remote-owned fields. `id`, `host`, `window` and `open`
    /// are left exactly as they were — the first two because the client's entry
    /// is the thing being *pointed* by them, the last two because they are this
    /// machine's view state and the remote has no opinion about them.
    pub fn apply_remote_json(&mut self, value: &serde_json::Value) -> serde_json::Result<()> {
        let record: RemoteRecord = serde_json::from_value(value.clone())?;
        self.name = record.name;
        self.session = record.session;
        self.last_active = record.last_active;
        Ok(())
    }
}

/// The `Workspace` fields the **remote** is the authority for.
/// Everything else is client-side view state and never leaves this machine.
///
/// A `Workspace` field that is in neither list is a bug: it would be dropped by
/// [`Workspace::to_remote_json`] and silently lost on the next pull. The test
/// `the_storage_split_covers_every_workspace_field` is what makes that a red
/// build rather than a data-loss report.
pub const REMOTE_OWNED_FIELDS: &[&str] = &["id", "name", "session", "last_active"];

/// The client-side view state, which stays in this machine's `session.json`.
/// `daemon_instance` is client-owned because it records **which serving process
/// this client last saw** — an observation, not a property of the workspace. Two
/// clients open on one remote workspace each keep their own, and neither may
/// overwrite the other's; a remote record that carried it would do exactly that.
pub const CLIENT_OWNED_FIELDS: &[&str] = &["window", "open", "host", "daemon_instance"];

/// The remote-owned half of a [`Workspace`], for reading a record back.
///
/// Every field defaults: a record written by a *newer* client carries fields
/// this build has never heard of (serde ignores them), and one written by an
/// older client is missing fields this build expects. Neither may fail the pull
/// — a workspace that will not decode is a workspace the user cannot open.
#[derive(Deserialize)]
struct RemoteRecord {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    session: Session,
    #[serde(default)]
    last_active: u64,
}

/// The whole `session.json`: every workspace tty7 knows about, plus which one
/// had focus at quit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Workspaces {
    /// Note: deliberately *not* `#[serde(default)]` at the struct level — the
    /// presence of this key is what distinguishes a new-format file from the
    /// legacy flat `{active, tabs}` one. See [`Workspaces::decode`].
    pub workspaces: Vec<Workspace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<WorkspaceId>,
}

impl Workspaces {
    /// Load every saved workspace. Returns `None` when the file is absent or
    /// unreadable (normal first run), and `None` with a warning when it fails
    /// to parse — never panics.
    pub fn load() -> Option<Self> {
        let path = Self::path()?;
        let text = std::fs::read_to_string(&path).ok()?;
        match Self::decode(&text) {
            Ok(loaded) => Some(loaded),
            Err(e) => {
                log::warn!(
                    "failed to parse session at {}: {e}; ignoring",
                    path.display()
                );
                None
            }
        }
    }

    /// Parse either format. A file written by any build with multi-window
    /// support has a `workspaces` array; anything else is a pre-multi-window
    /// `{active, tabs}` session, which migrates to a single open workspace so
    /// upgrading users keep their tabs (and their attached daemon panes).
    pub fn decode(text: &str) -> Result<Self, serde_json::Error> {
        let value: serde_json::Value = serde_json::from_str(crate::core::config::strip_bom(text))?;
        if value.get("workspaces").is_some() {
            return serde_json::from_value(value);
        }
        let legacy: Session = serde_json::from_value(value)?;
        Ok(Self::single(Workspace::from_session(legacy)))
    }

    /// A one-workspace set, used by the legacy migration and by first run.
    pub fn single(workspace: Workspace) -> Self {
        Self {
            active: Some(workspace.id),
            workspaces: vec![workspace],
        }
    }

    pub fn get(&self, id: WorkspaceId) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    pub fn get_mut(&mut self, id: WorkspaceId) -> Option<&mut Workspace> {
        self.workspaces.iter_mut().find(|w| w.id == id)
    }

    /// The workspaces that had a window at the last quit, in their saved order.
    ///
    /// Note that launch does **not** restore all of these — see
    /// [`workspace_to_restore`](Self::workspace_to_restore). They are still the
    /// set that matters here, because every one of them is holding live daemon
    /// panes and none of them may be forgotten.
    pub fn open_workspaces(&self) -> impl Iterator<Item = &Workspace> {
        self.workspaces.iter().filter(|w| w.open)
    }

    /// The one workspace launch comes up on: whichever the user was last in.
    ///
    /// Deliberately one, not all of them. Restoring every window that existed
    /// at quit means a four-window session costs four windows, four daemon
    /// attaches and four layout restores before the user has said what they
    /// want to do — and in practice they came back for *one* of them. The
    /// others are not lost by any measure that matters: their panes never
    /// stopped running in the daemon, and the switcher lists them a click away.
    ///
    /// [`active`](Self::active) is the answer whenever it is still open, since
    /// it is written on every focus change and so names the window that had the
    /// user's attention last. `last_active` is the fallback for a store written
    /// by a build that did not track focus, or one whose active workspace was
    /// closed before quitting.
    pub fn workspace_to_restore(&self) -> Option<WorkspaceId> {
        let focused = self
            .active
            .filter(|id| self.get(*id).is_some_and(|w| w.open));
        focused.or_else(|| {
            self.open_workspaces()
                .max_by_key(|w| w.last_active)
                .map(|w| w.id)
        })
    }

    /// Closed workspaces for the home-page picker, most recently active first.
    pub fn closed_workspaces(&self) -> Vec<&Workspace> {
        let mut closed: Vec<&Workspace> = self.workspaces.iter().filter(|w| !w.open).collect();
        closed.sort_by(|a, b| b.last_active.cmp(&a.last_active));
        closed
    }

    /// Drop pane ids that appear in more than one workspace *on the same
    /// machine*, keeping the claim of whichever workspace was active most
    /// recently. A duplicate would have two windows attach the same daemon
    /// pane, and the daemon's single subscriber means the loser's terminal goes
    /// silently dead — so this runs on every load, before any window is built.
    ///
    /// **Scoped per machine, because a pane id only means anything within one
    /// daemon.** Every daemon hands out 1, 2, 3…, so a laptop and a build box
    /// both having a pane 1 is the normal case, not a conflict. Deduping
    /// globally would make the remote workspace forfeit a claim on a pane that
    /// is alive and well on its own machine — orphaning a live session over a
    /// collision that never existed.
    ///
    /// Returns the number of claims dropped (0 in the healthy case).
    pub fn dedupe_pane_ids(&mut self) -> usize {
        let mut order: Vec<(usize, u64)> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(i, w)| (i, w.last_active))
            .collect();
        // Most recently active first: it keeps its claim, earlier ones yield.
        order.sort_by(|a, b| b.1.cmp(&a.1));

        // One `seen` set per machine. `HostId` is process-local, but this only
        // has to be self-consistent within the single pass below.
        let mut seen: std::collections::HashMap<
            crate::host::HostId,
            std::collections::HashSet<u64>,
        > = std::collections::HashMap::new();
        let mut dropped = 0;
        for (index, _) in order {
            let workspace = &mut self.workspaces[index];
            let host = workspace.host_id();
            let seen_here = seen.entry(host).or_default();
            for tab in &mut workspace.session.tabs {
                dropped += drop_duplicate_pane_ids(&mut tab.pane, seen_here);
            }
        }
        dropped
    }

    /// Persist as JSON, creating the parent directory if needed. Any
    /// IO/serialization error is logged and swallowed — the app must never
    /// crash or stall over session bookkeeping.
    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::warn!("failed to create session dir {}: {e}", parent.display());
                return;
            }
        }
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("failed to serialize session: {e}");
                return;
            }
        };
        if let Err(e) = crate::core::config::write_atomic(&path, json.as_bytes()) {
            log::warn!("failed to write session to {}: {e}", path.display());
        }
    }

    /// `~/.config/tty7/session.json`, alongside `config.json`.
    fn path() -> Option<PathBuf> {
        crate::core::config::config_path("session.json")
    }
}

/// Seconds since the Unix epoch, or 0 if the clock is before it (which only a
/// badly misconfigured machine reports — "never active" is a fine reading).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Last path component as a display string, skipping a bare `/` or a path that
/// ends in `..`.
fn basename(path: &std::path::Path) -> Option<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

fn first_leaf_cwd(pane: &SessionPane) -> Option<PathBuf> {
    match pane {
        SessionPane::Leaf { cwd, .. } => cwd.clone(),
        SessionPane::Split { a, b, .. } => first_leaf_cwd(a).or_else(|| first_leaf_cwd(b)),
    }
}

fn leaf_count(pane: &SessionPane) -> usize {
    match pane {
        SessionPane::Leaf { .. } => 1,
        SessionPane::Split { a, b, .. } => leaf_count(a) + leaf_count(b),
    }
}

fn collect_pane_ids(pane: &SessionPane, out: &mut Vec<u64>) {
    match pane {
        SessionPane::Leaf { pane_id, .. } => out.extend(pane_id),
        SessionPane::Split { a, b, .. } => {
            collect_pane_ids(a, out);
            collect_pane_ids(b, out);
        }
    }
}

/// Blank every leaf's `pane_id` under `pane`, answering how many were set.
/// See [`Workspace::forget_pane_ids`].
pub fn blank_pane_ids(pane: &mut SessionPane) -> usize {
    match pane {
        SessionPane::Leaf { pane_id, .. } => usize::from(pane_id.take().is_some()),
        SessionPane::Split { a, b, .. } => blank_pane_ids(a) + blank_pane_ids(b),
    }
}

/// Blank any `pane_id` already claimed by an earlier-visited workspace. A
/// blanked leaf still restores — it just spawns a fresh shell in its saved cwd,
/// the same path a session from before the daemon existed takes.
///
/// The agent resume fields go with it. A blanked leaf takes restore's
/// spawn-fresh path, and that path auto-types the agent's resume command —
/// but the pane this claim duplicated is still running that very agent under
/// its winning workspace, so "recovering" the loser would start a second
/// process on the same agent session id. The duplicate claim is the evidence
/// of a corrupted record, not of a lost conversation; the conversation lives
/// with the winner.
fn drop_duplicate_pane_ids(
    pane: &mut SessionPane,
    seen: &mut std::collections::HashSet<u64>,
) -> usize {
    match pane {
        SessionPane::Leaf {
            pane_id,
            agent_session_id,
            agent_launch_argv,
            ..
        } => match *pane_id {
            Some(id) if !seen.insert(id) => {
                log::warn!(
                    "workspace claims pane {id} twice; dropping the duplicate claim \
                     (and its agent resume, which the winning claim still owns)"
                );
                *pane_id = None;
                *agent_session_id = None;
                *agent_launch_argv = None;
                1
            }
            _ => 0,
        },
        SessionPane::Split { a, b, .. } => {
            drop_duplicate_pane_ids(a, seen) + drop_duplicate_pane_ids(b, seen)
        }
    }
}

/// Helpers for every test that touches the on-disk `session.json`. The
/// config-dir pin is process-wide (`set_config_dir` is first-call-wins), so
/// the file is process-wide too — any test that reads or writes it must hold
/// [`lock_session_file`] across the whole read/write sequence, or parallel
/// tests clobber each other's session.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    static SESSION_FILE: Mutex<()> = Mutex::new(());

    /// Serialize access to the shared `session.json`.
    pub(crate) fn lock_session_file() -> MutexGuard<'static, ()> {
        // A poisoned lock just means another test failed mid-sequence; every
        // holder rewrites the file from scratch, so the state is still sound.
        SESSION_FILE.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Pin the process config dir at a shared temp location so `save`/`load`
    /// (which resolve `session.json` under it) never touch the real `~/.config`.
    /// `set_config_dir` is first-call-wins; every caller computes the same path.
    pub(crate) fn pin_config_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tty7-covtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        crate::core::config::set_config_dir(dir.clone());
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{lock_session_file, pin_config_dir};
    use super::*;

    #[test]
    fn session_json_round_trips_nested_tree() {
        let session = Session {
            active: 1,
            tabs: vec![
                SessionTab {
                    name: Some("build".into()),
                    sidebar_group: None,
                    pane: SessionPane::Leaf {
                        cwd: Some(PathBuf::from("/work")),
                        pane_id: Some(7),
                        ssh_spec: None,
                        agent: None,
                        agent_session_id: None,
                        agent_launch_argv: None,
                    },
                },
                SessionTab {
                    name: None,
                    sidebar_group: None,
                    pane: SessionPane::Split {
                        axis: SessionAxis::Vertical,
                        ratio: 0.3,
                        a: Box::new(SessionPane::Leaf {
                            cwd: None,
                            pane_id: None,
                            ssh_spec: None,
                            agent: None,
                            agent_session_id: None,
                            agent_launch_argv: None,
                        }),
                        b: Box::new(SessionPane::Leaf {
                            cwd: Some(PathBuf::from("/tmp")),
                            pane_id: Some(9),
                            ssh_spec: None,
                            agent: None,
                            agent_session_id: None,
                            agent_launch_argv: None,
                        }),
                    },
                },
            ],
        };
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.active, 1);
        assert_eq!(back.tabs.len(), 2);
        assert!(matches!(
            back.tabs[0].pane,
            SessionPane::Leaf {
                pane_id: Some(7),
                ..
            }
        ));
        match &back.tabs[1].pane {
            SessionPane::Split { ratio, .. } => assert!((ratio - 0.3).abs() < 1e-6),
            _ => panic!("expected a split"),
        }
    }

    #[test]
    fn leaf_agent_resume_fields_round_trip_and_default() {
        // Round trip: the agent + native session id survive serialization.
        let leaf = SessionPane::Leaf {
            cwd: None,
            pane_id: None,
            ssh_spec: None,
            agent: Some(crate::core::cli_agent::CLIAgent::Claude),
            agent_session_id: Some("abc-123".into()),
            agent_launch_argv: Some(vec![
                "claude".into(),
                "--dangerously-skip-permissions".into(),
            ]),
        };
        let back: SessionPane =
            serde_json::from_str(&serde_json::to_string(&leaf).unwrap()).unwrap();
        match back {
            SessionPane::Leaf {
                agent,
                agent_session_id,
                agent_launch_argv,
                ..
            } => {
                assert_eq!(agent, Some(crate::core::cli_agent::CLIAgent::Claude));
                assert_eq!(agent_session_id.as_deref(), Some("abc-123"));
                assert_eq!(
                    agent_launch_argv.as_deref(),
                    Some(
                        &[
                            "claude".to_string(),
                            "--dangerously-skip-permissions".to_string()
                        ][..]
                    )
                );
            }
            _ => panic!("expected leaf"),
        }
        // A session written before these fields existed decodes with `None`s.
        let old: SessionPane =
            serde_json::from_str(r#"{"Leaf":{"cwd":"/x","pane_id":3}}"#).unwrap();
        assert!(matches!(
            old,
            SessionPane::Leaf {
                agent: None,
                agent_session_id: None,
                agent_launch_argv: None,
                ..
            }
        ));
    }

    #[test]
    fn a_utf8_bom_does_not_discard_the_session() {
        // `Session::load` treats a parse error as "no session", so a BOM on a
        // hand-edited `session.json` doesn't warn — it drops every workspace
        // and opens on the home page as if nothing had been saved.
        // Legacy `{active, tabs}` shape, so this also covers the migration path.
        let decoded = Workspaces::decode(
            "\u{FEFF}{\"active\": 0, \"tabs\": [{\"pane\": {\"Leaf\": {\"cwd\": \"/work\"}}}]}",
        )
        .expect("a BOM'd session still decodes");
        let tabs = &decoded
            .workspaces
            .first()
            .expect("migrated workspace")
            .session
            .tabs;
        assert_eq!(tabs.len(), 1);
    }

    #[test]
    fn session_defaults_fill_missing_fields() {
        // An empty object → default (active 0, no tabs).
        let s: Session = serde_json::from_str("{}").unwrap();
        assert_eq!(s.active, 0);
        assert!(s.tabs.is_empty());

        // A split without a ratio falls back to the 0.5 default, and a leaf
        // without cwd/pane_id decodes with `None`s.
        let pane: SessionPane = serde_json::from_str(
            r#"{"Split":{"axis":"Horizontal","a":{"Leaf":{}},"b":{"Leaf":{}}}}"#,
        )
        .unwrap();
        match pane {
            SessionPane::Split { ratio, .. } => assert_eq!(ratio, 0.5),
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn save_then_load_recovers_the_session() {
        let _file = lock_session_file();
        pin_config_dir();
        let session = Session {
            active: 0,
            tabs: vec![SessionTab {
                name: Some("main".into()),
                sidebar_group: None,
                pane: SessionPane::Leaf {
                    cwd: Some(PathBuf::from("/home/u")),
                    pane_id: Some(1),
                    ssh_spec: None,
                    agent: None,
                    agent_session_id: None,
                    agent_launch_argv: None,
                },
            }],
        };
        Workspaces::single(Workspace::from_session(session)).save();
        let loaded = Workspaces::load().expect("a saved session should load back");
        let only = &loaded.workspaces[0];
        assert_eq!(only.session.tabs.len(), 1);
        assert_eq!(only.session.tabs[0].name.as_deref(), Some("main"));
        assert_eq!(loaded.active, Some(only.id));
    }

    // ── Workspace layer ─────────────────────────────────────────────────────

    /// Build a leaf with the given cwd + pane id; the agent/ssh fields are
    /// irrelevant to every workspace-layer test.
    fn leaf(cwd: Option<&str>, pane_id: Option<u64>) -> SessionPane {
        SessionPane::Leaf {
            cwd: cwd.map(PathBuf::from),
            pane_id,
            ssh_spec: None,
            agent: None,
            agent_session_id: None,
            agent_launch_argv: None,
        }
    }

    fn tab(pane: SessionPane, group: Option<&str>) -> SessionTab {
        SessionTab {
            name: None,
            sidebar_group: group.map(PathBuf::from),
            pane,
        }
    }

    fn workspace(tabs: Vec<SessionTab>) -> Workspace {
        Workspace::from_session(Session { active: 0, tabs })
    }

    #[test]
    fn legacy_flat_session_migrates_to_one_open_workspace() {
        // Exactly the shape every pre-multi-window build wrote.
        let legacy = r#"{"active":1,"tabs":[
            {"name":"build","pane":{"Leaf":{"cwd":"/work","pane_id":7}}},
            {"name":null,"pane":{"Leaf":{"cwd":"/tmp","pane_id":9}}}
        ]}"#;
        let loaded = Workspaces::decode(legacy).expect("legacy session should migrate");
        assert_eq!(loaded.workspaces.len(), 1);
        let only = &loaded.workspaces[0];
        // The tabs — and crucially the pane ids, which are live daemon panes —
        // survive the upgrade, so an updating user doesn't lose their shells.
        assert_eq!(only.session.active, 1);
        assert_eq!(only.session.tabs.len(), 2);
        assert_eq!(only.pane_ids(), vec![7, 9]);
        // It reopens on the next launch, matching pre-upgrade behavior.
        assert!(only.open);
        assert_eq!(loaded.active, Some(only.id));
    }

    #[test]
    fn empty_and_absent_shapes_decode_without_losing_data() {
        // `{}` is the home-page state an older build wrote: zero tabs, still valid.
        let empty = Workspaces::decode("{}").expect("empty object decodes");
        assert_eq!(empty.workspaces.len(), 1);
        assert!(empty.workspaces[0].session.tabs.is_empty());
        // A new-format file with no workspaces at all stays empty rather than
        // being mistaken for a legacy session and gaining a phantom entry.
        let none = Workspaces::decode(r#"{"workspaces":[]}"#).expect("new format decodes");
        assert!(none.workspaces.is_empty());
    }

    #[test]
    fn new_format_round_trips_through_json() {
        let mut ws = workspace(vec![tab(leaf(Some("/work"), Some(3)), Some("/work"))]);
        ws.name = Some("api".into());
        ws.open = false;
        ws.last_active = 1_700_000_000;
        let id = ws.id;
        let all = Workspaces {
            active: Some(id),
            workspaces: vec![ws],
        };
        let back = Workspaces::decode(&serde_json::to_string(&all).unwrap()).unwrap();
        let only = &back.workspaces[0];
        assert_eq!(only.id, id, "workspace identity must survive a restart");
        assert_eq!(only.name.as_deref(), Some("api"));
        assert!(!only.open);
        assert_eq!(only.last_active, 1_700_000_000);
        assert_eq!(back.active, Some(id));
    }

    /// Ending a workspace's sessions leaves the layout and drops the ids — the
    /// cwds are what reopening rebuilds from, and a kept id would send restore
    /// down the reattach path to a pane that no longer exists.
    #[test]
    fn forgetting_pane_ids_keeps_the_layout_and_the_cwds() {
        let mut ws = workspace(vec![
            tab(
                SessionPane::Split {
                    axis: SessionAxis::Horizontal,
                    ratio: 0.5,
                    a: Box::new(leaf(Some("/work"), Some(1))),
                    b: Box::new(leaf(Some("/work/api"), Some(2))),
                },
                Some("/work"),
            ),
            tab(leaf(Some("/tmp"), None), None),
        ]);

        assert_eq!(
            ws.forget_pane_ids(),
            2,
            "only the claims that existed count"
        );
        assert!(ws.pane_ids().is_empty());
        assert_eq!(ws.session.tabs.len(), 2, "the tabs are what survives");
        assert_eq!(ws.pane_count(), 3, "and so is the split");
        assert_eq!(
            ws.first_cwd(),
            Some(PathBuf::from("/work")),
            "reopening respawns in the saved directory, so it must still be there"
        );
        assert_eq!(
            ws.forget_pane_ids(),
            0,
            "a second pass has nothing to do, so the caller can skip its write"
        );
    }

    /// The stale-instance check: ids recorded against a *different* daemon
    /// process are blanked (they now name unrelated shells at best), ids
    /// recorded against the *same* one are kept, and an unknown on either side
    /// changes nothing — treating "cannot tell" as stale would respawn every
    /// pane on the first launch after an upgrade.
    #[test]
    fn stale_instance_blanks_pane_ids_and_matching_or_unknown_keeps_them() {
        let fresh = |instance: Option<&str>| {
            let mut ws = workspace(vec![tab(leaf(Some("/work"), Some(7)), None)]);
            ws.daemon_instance = instance.map(str::to_string);
            ws
        };

        let mut ws = fresh(Some("daemon-a"));
        assert_eq!(ws.forget_stale_pane_ids(Some("daemon-b")), 1);
        assert!(ws.pane_ids().is_empty());
        assert_eq!(
            ws.first_cwd(),
            Some(PathBuf::from("/work")),
            "the layout survives; only the claims go"
        );

        let mut ws = fresh(Some("daemon-a"));
        assert_eq!(ws.forget_stale_pane_ids(Some("daemon-a")), 0);
        assert_eq!(ws.pane_ids(), vec![7], "same process, ids stay attachable");

        let mut ws = fresh(None);
        assert_eq!(ws.forget_stale_pane_ids(Some("daemon-b")), 0);
        assert_eq!(ws.pane_ids(), vec![7], "an old record is not judged");

        let mut ws = fresh(Some("daemon-a"));
        assert_eq!(ws.forget_stale_pane_ids(None), 0);
        assert_eq!(ws.pane_ids(), vec![7], "an unknown daemon is not judged");
    }

    /// Unlike a duplicate claim, a stale-instance claim keeps its agent resume:
    /// the daemon those panes lived in is gone, nothing else runs the
    /// conversation, and the fresh shell resuming it is the feature working.
    #[test]
    fn stale_instance_keeps_the_agent_resume() {
        let mut ws = workspace(vec![tab(
            SessionPane::Leaf {
                cwd: Some(PathBuf::from("/work")),
                pane_id: Some(7),
                ssh_spec: None,
                agent: Some(crate::core::cli_agent::CLIAgent::Claude),
                agent_session_id: Some("sid".into()),
                agent_launch_argv: None,
            },
            None,
        )]);
        ws.daemon_instance = Some("daemon-a".into());
        assert_eq!(ws.forget_stale_pane_ids(Some("daemon-b")), 1);
        match &ws.session.tabs[0].pane {
            SessionPane::Leaf {
                pane_id,
                agent_session_id,
                ..
            } => {
                assert!(pane_id.is_none());
                assert_eq!(agent_session_id.as_deref(), Some("sid"));
            }
            SessionPane::Split { .. } => panic!("leaf stays a leaf"),
        }
    }

    #[test]
    fn display_name_prefers_user_name_then_repo_then_cwd() {
        // No name, no repo group: fall back to the first leaf's directory.
        let ws = workspace(vec![tab(leaf(Some("/home/u/scratch"), None), None)]);
        assert_eq!(ws.display_name(), "scratch");

        // A repo group wins over the cwd — it's the workspace's real subject.
        let ws = workspace(vec![tab(
            leaf(Some("/repo/tty7/src"), None),
            Some("/repo/tty7"),
        )]);
        assert_eq!(ws.display_name(), "tty7");

        // The majority repo wins when tabs straddle two checkouts.
        let ws = workspace(vec![
            tab(leaf(None, None), Some("/repo/other")),
            tab(leaf(None, None), Some("/repo/tty7")),
            tab(leaf(None, None), Some("/repo/tty7")),
        ]);
        assert_eq!(ws.display_name(), "tty7");

        // An explicit name beats everything derived.
        let mut ws = workspace(vec![tab(
            leaf(Some("/repo/tty7"), None),
            Some("/repo/tty7"),
        )]);
        ws.name = Some("  Release prep  ".into());
        assert_eq!(ws.display_name(), "Release prep");

        // Nothing to go on at all.
        assert_eq!(workspace(vec![]).display_name(), "Untitled");
        // A whitespace-only name is treated as unset rather than rendering blank.
        let mut ws = workspace(vec![tab(leaf(Some("/x/proj"), None), None)]);
        ws.name = Some("   ".into());
        assert_eq!(ws.display_name(), "proj");
    }

    #[test]
    fn pane_and_tab_counts_walk_the_split_tree() {
        let ws = workspace(vec![
            tab(leaf(Some("/a"), Some(1)), None),
            tab(
                SessionPane::Split {
                    axis: SessionAxis::Vertical,
                    ratio: 0.5,
                    a: Box::new(leaf(Some("/b"), Some(2))),
                    b: Box::new(leaf(None, Some(3))),
                },
                None,
            ),
        ]);
        assert_eq!(ws.pane_count(), 3);
        assert_eq!(ws.pane_ids(), vec![1, 2, 3]);
        assert_eq!(ws.first_cwd(), Some(PathBuf::from("/a")));
    }

    #[test]
    fn dedupe_pane_ids_keeps_the_most_recently_active_claim() {
        // Two workspaces both claim pane 5 — the crash/hand-edit case. The
        // stale one must yield, or its window silently steals the live one's
        // stream when both attach (the daemon has a single subscriber).
        let mut stale = workspace(vec![tab(leaf(Some("/old"), Some(5)), None)]);
        stale.last_active = 100;
        let mut fresh = workspace(vec![tab(leaf(Some("/new"), Some(5)), None)]);
        fresh.last_active = 200;
        let (stale_id, fresh_id) = (stale.id, fresh.id);

        let mut all = Workspaces {
            active: Some(fresh_id),
            workspaces: vec![stale, fresh],
        };
        assert_eq!(all.dedupe_pane_ids(), 1);

        // The recent one keeps pane 5; the stale one drops to a fresh spawn in
        // its saved cwd (cwd is preserved — only the id is cleared).
        assert_eq!(all.get(fresh_id).unwrap().pane_ids(), vec![5]);
        assert!(all.get(stale_id).unwrap().pane_ids().is_empty());
        assert_eq!(
            all.get(stale_id).unwrap().first_cwd(),
            Some(PathBuf::from("/old"))
        );
    }

    /// The duplicate claim loses its agent resume along with its pane id.
    /// Restore's spawn-fresh path auto-types the agent's resume command, and
    /// the winning workspace's pane is still *running* that agent — a loser
    /// that kept `agent_session_id` would come back as a second process on
    /// the same conversation (double `claude --resume <id>`, both live).
    #[test]
    fn dedupe_pane_ids_disarms_the_duplicate_claims_agent_resume() {
        let agent_leaf = |pane_id| SessionPane::Leaf {
            cwd: Some(PathBuf::from("/work")),
            pane_id: Some(pane_id),
            ssh_spec: None,
            agent: Some(crate::core::cli_agent::CLIAgent::Claude),
            agent_session_id: Some("362f9261".into()),
            agent_launch_argv: Some(vec!["claude".into(), "--continue".into()]),
        };
        let mut stale = workspace(vec![tab(agent_leaf(5), None)]);
        stale.last_active = 100;
        let mut fresh = workspace(vec![tab(agent_leaf(5), None)]);
        fresh.last_active = 200;
        let (stale_id, fresh_id) = (stale.id, fresh.id);

        let mut all = Workspaces {
            active: Some(fresh_id),
            workspaces: vec![stale, fresh],
        };
        assert_eq!(all.dedupe_pane_ids(), 1);

        let loser = &all.get(stale_id).unwrap().session.tabs[0].pane;
        match loser {
            SessionPane::Leaf {
                pane_id,
                cwd,
                agent_session_id,
                agent_launch_argv,
                ..
            } => {
                assert!(pane_id.is_none());
                assert_eq!(
                    cwd.as_deref(),
                    Some(std::path::Path::new("/work")),
                    "the layout survives — only the claim and its resume go"
                );
                assert!(
                    agent_session_id.is_none(),
                    "no second resume of one conversation"
                );
                assert!(agent_launch_argv.is_none());
            }
            SessionPane::Split { .. } => panic!("the leaf must survive as a leaf"),
        }

        // The winner is untouched: its pane is the one actually running the agent.
        match &all.get(fresh_id).unwrap().session.tabs[0].pane {
            SessionPane::Leaf {
                pane_id,
                agent_session_id,
                ..
            } => {
                assert_eq!(*pane_id, Some(5));
                assert_eq!(agent_session_id.as_deref(), Some("362f9261"));
            }
            SessionPane::Split { .. } => panic!("the leaf must survive as a leaf"),
        }
    }

    /// A pane id is only unique within one daemon, so the same number on two
    /// machines is not a collision. Deduping globally would make the remote
    /// workspace forfeit a claim on a pane that is alive on its own box —
    /// orphaning a live session over a conflict that never existed.
    #[test]
    fn dedupe_pane_ids_is_scoped_to_one_machine() {
        let mut local = workspace(vec![tab(leaf(Some("/local"), Some(1)), None)]);
        local.last_active = 200;
        let mut remote = workspace(vec![tab(leaf(Some("/remote"), Some(1)), None)]);
        remote.last_active = 100; // older, so a global dedupe would drop *this* one
        remote.host = Some(RemoteRef {
            target: RemoteTarget::Alias {
                alias: "build-box".into(),
            },
            workspace: WorkspaceId::new(),
        });
        let (local_id, remote_id) = (local.id, remote.id);

        let mut all = Workspaces {
            active: Some(local_id),
            workspaces: vec![local, remote],
        };
        assert_eq!(all.dedupe_pane_ids(), 0, "different machines never collide");
        assert_eq!(all.get(local_id).unwrap().pane_ids(), vec![1]);
        assert_eq!(
            all.get(remote_id).unwrap().pane_ids(),
            vec![1],
            "the remote keeps its claim on its own daemon's pane 1"
        );

        // …and two workspaces on the *same* remote machine still dedupe.
        let host = RemoteRef {
            target: RemoteTarget::Alias {
                alias: "build-box".into(),
            },
            workspace: WorkspaceId::new(),
        };
        let mut older = workspace(vec![tab(leaf(Some("/a"), Some(7)), None)]);
        older.last_active = 100;
        older.host = Some(host.clone());
        let mut newer = workspace(vec![tab(leaf(Some("/b"), Some(7)), None)]);
        newer.last_active = 200;
        newer.host = Some(host);
        let (older_id, newer_id) = (older.id, newer.id);

        let mut same_box = Workspaces {
            active: Some(newer_id),
            workspaces: vec![older, newer],
        };
        assert_eq!(same_box.dedupe_pane_ids(), 1);
        assert_eq!(same_box.get(newer_id).unwrap().pane_ids(), vec![7]);
        assert!(same_box.get(older_id).unwrap().pane_ids().is_empty());
    }

    #[test]
    fn dedupe_pane_ids_is_a_noop_on_healthy_sessions() {
        let mut all = Workspaces {
            active: None,
            workspaces: vec![
                workspace(vec![tab(leaf(Some("/a"), Some(1)), None)]),
                workspace(vec![tab(leaf(Some("/b"), Some(2)), None)]),
            ],
        };
        assert_eq!(all.dedupe_pane_ids(), 0);
        assert_eq!(all.workspaces[0].pane_ids(), vec![1]);
        assert_eq!(all.workspaces[1].pane_ids(), vec![2]);
    }

    #[test]
    fn dedupe_pane_ids_catches_a_duplicate_within_one_workspace() {
        // Same guarantee inside a single workspace: a split that somehow ended
        // up with the same pane in both halves would deadlock the same way.
        let mut all = Workspaces {
            active: None,
            workspaces: vec![workspace(vec![
                tab(leaf(Some("/a"), Some(1)), None),
                tab(leaf(Some("/b"), Some(1)), None),
            ])],
        };
        assert_eq!(all.dedupe_pane_ids(), 1);
        assert_eq!(all.workspaces[0].pane_ids(), vec![1]);
    }

    // ── Remote workspaces (M5) ──────────────────────────────────────────────

    /// A real-shaped `session.json` from before `host` existed, written by the
    /// build that shipped multi-window. **The hard requirement of the whole
    /// field**: every workspace in it is local, and every derived answer is
    /// exactly what it was — an upgrading user's file must not acquire a
    /// meaning it did not have.
    const LEGACY_SESSION_JSON: &str = r#"{
      "workspaces": [
        {
          "id": "6a8f2a1e-1c1b-4f7a-9d3e-2b5c8e4a7f01",
          "name": "tty7",
          "session": {
            "active": 1,
            "tabs": [
              {
                "name": "build",
                "pane": {"Leaf": {"cwd": "/Users/me/repo/tty7", "pane_id": 41}},
                "sidebar_group": "/Users/me/repo/tty7"
              },
              {
                "name": null,
                "pane": {"Split": {
                  "axis": "Vertical",
                  "ratio": 0.35,
                  "a": {"Leaf": {"cwd": "/Users/me/repo/tty7/src", "pane_id": 42,
                                 "agent": "Claude", "agent_session_id": "s-9"}},
                  "b": {"Leaf": {"cwd": "/Users/me/repo/tty7", "pane_id": 43}}
                }},
                "sidebar_group": "/Users/me/repo/tty7"
              }
            ]
          },
          "window": {"x": 120.0, "y": 64.0, "width": 1440.0, "height": 900.0},
          "open": true,
          "last_active": 1753600000
        },
        {
          "id": "7b9e3b2f-2d2c-4a8b-8e4f-3c6d9f5b8a12",
          "session": {"active": 0, "tabs": [
            {"pane": {"Leaf": {"cwd": "/Users/me/scratch"}}}
          ]},
          "open": false,
          "last_active": 1753500000
        }
      ],
      "active": "6a8f2a1e-1c1b-4f7a-9d3e-2b5c8e4a7f01"
    }"#;

    #[test]
    fn an_old_session_json_is_all_local_and_behaves_identically() {
        let loaded = Workspaces::decode(LEGACY_SESSION_JSON).expect("an old session must decode");
        assert_eq!(loaded.workspaces.len(), 2);

        for ws in &loaded.workspaces {
            assert!(ws.host.is_none(), "a file without `host` decodes as local");
            assert!(!ws.is_remote());
            assert_eq!(
                ws.host_id(),
                crate::host::HostId::LOCAL,
                "no `host` must mean this machine, not a derived id"
            );
        }

        // Every derived answer is what the pre-`host` build gave.
        let first = &loaded.workspaces[0];
        assert_eq!(first.display_name(), "tty7");
        assert_eq!(first.pane_ids(), vec![41, 42, 43]);
        assert_eq!(first.pane_count(), 3);
        assert_eq!(
            first.dominant_repo(),
            Some(PathBuf::from("/Users/me/repo/tty7"))
        );
        assert!(first.open);
        assert_eq!(first.last_active, 1_753_600_000);
        assert!(first.window.is_some());
        assert_eq!(loaded.workspaces[1].display_name(), "scratch");
        assert!(!loaded.workspaces[1].open);
        assert_eq!(loaded.active, Some(loaded.workspaces[0].id));

        // And writing it back does not add a `host` key: a local workspace's
        // serialization is byte-for-byte what it always was, so downgrading to
        // an older build is not a one-way door either.
        let text = serde_json::to_string(&loaded).unwrap();
        assert!(!text.contains("\"host\""), "{text}");
        // Re-decoding the round trip changes nothing.
        let again = Workspaces::decode(&text).unwrap();
        assert_eq!(again.workspaces[0].pane_ids(), vec![41, 42, 43]);
        assert!(again.workspaces.iter().all(|w| !w.is_remote()));
    }

    /// The legacy flat `{active, tabs}` shape — two formats older — migrates to
    /// a local workspace too, not to one with a phantom host.
    #[test]
    fn the_pre_multi_window_migration_is_local() {
        let loaded =
            Workspaces::decode(r#"{"active":0,"tabs":[{"pane":{"Leaf":{"cwd":"/w"}}}]}"#).unwrap();
        assert!(!loaded.workspaces[0].is_remote());
        assert_eq!(loaded.workspaces[0].host_id(), crate::host::HostId::LOCAL);
    }

    /// The four key formats of the connection key, verbatim. These strings are a
    /// wire contract in all but name: change one and every workspace on that
    /// machine gets a different `HostId` than the connection pool minted.
    #[test]
    fn connection_keys_match_the_contract_table() {
        let uuid = uuid::Uuid::parse_str("6a8f2a1e-1c1b-4f7a-9d3e-2b5c8e4a7f01").unwrap();
        assert_eq!(
            RemoteTarget::Profile { id: uuid }.connection_key(),
            "ssh-profile:6a8f2a1e-1c1b-4f7a-9d3e-2b5c8e4a7f01"
        );
        assert_eq!(
            RemoteTarget::Alias {
                alias: "devbox".into()
            }
            .connection_key(),
            "ssh-alias:devbox"
        );
        assert_eq!(
            RemoteTarget::direct("me", "box.local", 22).connection_key(),
            "ssh-direct:me@box.local:22"
        );
        assert_eq!(
            RemoteTarget::direct("me", "box.local", 2222).connection_key(),
            "ssh-direct:me@box.local:2222"
        );
        assert_eq!(
            RemoteTarget::Wsl {
                distro: "Ubuntu".into()
            }
            .connection_key(),
            "wsl:Ubuntu"
        );
    }

    #[test]
    fn direct_targets_normalize_and_reuse_the_quick_connect_parser() {
        // The port defaults to 22, the scheme is optional, and the host folds
        // case — all of it the connection manager's existing behaviour.
        assert_eq!(
            RemoteTarget::parse_direct("ssh://me@Box.Local"),
            Some(RemoteTarget::direct("me", "box.local", 22))
        );
        assert_eq!(
            RemoteTarget::parse_direct("me@box.local:2222"),
            Some(RemoteTarget::direct("me", "box.local", 2222))
        );
        // A hand-edited file with an uppercase host still derives one id.
        let shouty = RemoteTarget::Direct {
            user: "me".into(),
            host: "BOX.LOCAL".into(),
            port: 22,
        };
        assert_eq!(
            shouty.host_id(),
            RemoteTarget::direct("me", "box.local", 22).host_id()
        );
        // Rejected inputs stay rejected rather than becoming a half-target.
        assert_eq!(RemoteTarget::parse_direct(""), None);
        assert_eq!(RemoteTarget::parse_direct("me@box:0"), None);
        // An alias is *not* case-folded: `ssh Devbox` and `ssh devbox` match
        // different stanzas, and so must these.
        assert_ne!(
            RemoteTarget::Alias {
                alias: "Devbox".into()
            }
            .connection_key(),
            RemoteTarget::Alias {
                alias: "devbox".into()
            }
            .connection_key()
        );
    }

    /// The dev-only `--stdio` target is a *machine*, not a variation on local:
    /// its key is distinct, its id is not [`HostId::LOCAL`], and two different
    /// server binaries are two different machines.
    ///
    /// That last part matters because everything keyed by `HostId` — the
    /// connection pool, the git-status cache, the auth queue — would otherwise
    /// merge two servers that share nothing.
    #[test]
    fn a_local_stdio_target_is_its_own_machine() {
        let a = RemoteTarget::LocalStdio {
            program: "/opt/tty7-server".into(),
            args: vec!["--stdio".into()],
        };
        let b = RemoteTarget::LocalStdio {
            program: "/tmp/other-server".into(),
            args: vec!["--stdio".into()],
        };
        assert_eq!(a.connection_key(), "local-stdio:/opt/tty7-server --stdio");
        assert_ne!(a.host_id(), b.host_id());
        assert!(
            !a.host_id().is_local(),
            "a routed target is never the local host"
        );
        // The label is the binary's name, not the argv: the flags say nothing a
        // status bar can use.
        assert_eq!(a.to_string(), "local:tty7-server");
    }

    /// The granularity the connection pool depends on: one box, one id, however
    /// many workspaces — and never [`HostId::LOCAL`].
    #[test]
    fn workspaces_on_one_box_share_a_host_id() {
        let target = RemoteTarget::Alias {
            alias: "devbox".into(),
        };
        let a = Workspace::on_remote(RemoteRef::new(target.clone(), WorkspaceId::new()));
        let b = Workspace::on_remote(RemoteRef::new(target.clone(), WorkspaceId::new()));
        assert_ne!(
            a.host.as_ref().unwrap().workspace,
            b.host.as_ref().unwrap().workspace
        );
        assert_eq!(a.host_id(), b.host_id(), "same machine, one HostId");
        assert!(!a.host_id().is_local());

        // A different machine is a different id.
        let other = Workspace::on_remote(RemoteRef::new(
            RemoteTarget::Alias {
                alias: "other".into(),
            },
            WorkspaceId::new(),
        ));
        assert_ne!(a.host_id(), other.host_id());

        // And a remote entry starts with no layout: the remote owns it.
        assert!(a.session.tabs.is_empty());
        assert_eq!(
            a.host.as_ref().unwrap().store_key(),
            a.host.as_ref().unwrap().workspace.to_string()
        );
    }

    #[test]
    fn a_remote_workspace_survives_a_restart() {
        let remote_id = WorkspaceId::new();
        let mut ws = Workspace::on_remote(RemoteRef::new(
            RemoteTarget::direct("me", "box.local", 2222),
            remote_id,
        ));
        ws.name = Some("api".into());
        ws.open = false;
        let all = Workspaces {
            active: None,
            workspaces: vec![ws],
        };
        let text = serde_json::to_string(&all).unwrap();
        let back = Workspaces::decode(&text).unwrap();
        let only = &back.workspaces[0];
        assert!(only.is_remote());
        let host = only.host.as_ref().unwrap();
        assert_eq!(host.workspace, remote_id);
        assert_eq!(host.target, RemoteTarget::direct("me", "box.local", 2222));
        assert_eq!(host.target.connection_key(), "ssh-direct:me@box.local:2222");
    }

    /// Every `Workspace` field belongs to exactly one side of the storage
    /// split. A new field that is in neither list would be silently dropped by
    /// `to_remote_json` and lost on the next pull, which is data loss that no
    /// other test would notice.
    #[test]
    fn the_storage_split_covers_every_workspace_field() {
        let mut ws = workspace(vec![tab(leaf(Some("/w"), Some(1)), Some("/w"))]);
        ws.name = Some("named".into());
        ws.window = Some(crate::core::window_state::WindowState {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        });
        ws.host = Some(RemoteRef::new(
            RemoteTarget::Alias {
                alias: "devbox".into(),
            },
            WorkspaceId::new(),
        ));
        // Every skip-when-`None` field must be populated here, or it never
        // serializes and this census can't see it.
        ws.daemon_instance = Some("daemon-uuid".into());

        let value = serde_json::to_value(&ws).unwrap();
        let mut present: Vec<String> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::from)
            .collect();
        present.sort();
        let mut expected: Vec<String> = REMOTE_OWNED_FIELDS
            .iter()
            .chain(CLIENT_OWNED_FIELDS)
            .map(|s| (*s).to_string())
            .collect();
        expected.sort();
        assert_eq!(
            present, expected,
            "a Workspace field is on neither side of the storage split; decide which \
             machine owns it and add it to REMOTE_OWNED_FIELDS or CLIENT_OWNED_FIELDS"
        );
    }

    #[test]
    fn the_remote_record_carries_the_layout_and_nothing_local() {
        let mut ws = workspace(vec![tab(leaf(Some("/srv/app"), Some(7)), Some("/srv/app"))]);
        ws.name = Some("app".into());
        ws.last_active = 1_753_600_000;
        ws.open = true;
        ws.window = Some(crate::core::window_state::WindowState {
            x: 1.0,
            y: 2.0,
            width: 800.0,
            height: 600.0,
        });
        ws.host = Some(RemoteRef::new(
            RemoteTarget::Alias {
                alias: "devbox".into(),
            },
            WorkspaceId::new(),
        ));

        let record = ws.to_remote_json();
        let obj = record.as_object().unwrap();
        // The machine's facts go over.
        assert!(obj.contains_key("session"));
        assert_eq!(obj["name"], "app");
        assert_eq!(obj["last_active"], 1_753_600_000u64);
        assert_eq!(obj["id"], ws.id.to_string());
        // This client's view state does not — the point of the split.
        for k in CLIENT_OWNED_FIELDS {
            assert!(!obj.contains_key(*k), "`{k}` must not leave this machine");
        }

        // Pulling it back onto a *different* client's entry updates the layout
        // and leaves that client's own view state alone.
        let mut mine = Workspace::on_remote(RemoteRef::new(
            RemoteTarget::Alias {
                alias: "devbox".into(),
            },
            ws.id,
        ));
        mine.open = false;
        mine.window = None;
        let my_id = mine.id;
        mine.apply_remote_json(&record).unwrap();
        assert_eq!(mine.session.tabs.len(), 1);
        assert_eq!(mine.name.as_deref(), Some("app"));
        assert_eq!(mine.last_active, 1_753_600_000);
        assert_eq!(
            mine.id, my_id,
            "the client's own entry id is not overwritten"
        );
        assert!(
            !mine.open,
            "the remote has no opinion about my open windows"
        );
        assert!(mine.window.is_none());
        assert!(mine.is_remote(), "and it is still a remote workspace");
    }

    /// A record from a newer client carries fields this build has never seen,
    /// and one from an older client is missing fields it expects. Neither may
    /// fail the pull.
    #[test]
    fn applying_a_record_tolerates_version_skew() {
        let mut ws = Workspace::default();
        ws.apply_remote_json(&serde_json::json!({
            "id": "6a8f2a1e-1c1b-4f7a-9d3e-2b5c8e4a7f01",
            "session": {"active": 0, "tabs": []},
            "last_active": 5,
            "something_from_2027": {"nested": true}
        }))
        .expect("unknown fields are ignored, not fatal");
        assert_eq!(ws.last_active, 5);

        let mut ws = Workspace {
            name: Some("stale".into()),
            ..Workspace::default()
        };
        ws.apply_remote_json(&serde_json::json!({}))
            .expect("a record missing every optional field still applies");
        assert_eq!(
            ws.name, None,
            "the remote's answer wins, including 'no name'"
        );
        assert!(ws.session.tabs.is_empty());
    }

    #[test]
    fn open_and_closed_partition_by_flag_and_recency() {
        let mut open_one = workspace(vec![]);
        open_one.open = true;
        let mut older = workspace(vec![]);
        older.open = false;
        older.last_active = 100;
        let mut newer = workspace(vec![]);
        newer.open = false;
        newer.last_active = 300;
        let (open_id, older_id, newer_id) = (open_one.id, older.id, newer.id);

        let all = Workspaces {
            active: None,
            workspaces: vec![open_one, older, newer],
        };
        assert_eq!(
            all.open_workspaces().map(|w| w.id).collect::<Vec<_>>(),
            vec![open_id]
        );
        // The picker lists most-recently-active first.
        assert_eq!(
            all.closed_workspaces()
                .iter()
                .map(|w| w.id)
                .collect::<Vec<_>>(),
            vec![newer_id, older_id]
        );
    }

    /// Launch restores exactly one window, and it is the one the user was in.
    ///
    /// Pinned because the two inputs disagree on purpose: `active` is written on
    /// every focus change, so it is the truth even when some *other* window saw
    /// more recent activity (an agent finishing a build touches `last_active`
    /// without anybody looking at it).
    #[test]
    fn launch_restores_the_focused_workspace_not_the_most_recently_touched() {
        let mut focused = workspace(vec![]);
        focused.open = true;
        focused.last_active = 100;
        let mut busier = workspace(vec![]);
        busier.open = true;
        busier.last_active = 900;
        let (focused_id, busier_id) = (focused.id, busier.id);

        let all = Workspaces {
            active: Some(focused_id),
            workspaces: vec![focused, busier],
        };
        assert_eq!(all.workspace_to_restore(), Some(focused_id));
        assert_eq!(
            all.open_workspaces().count(),
            2,
            "the others stay open in the store — launch detaches them, this does not"
        );

        // No focus recorded (or it named a workspace that was closed first):
        // recency is the fallback, not a coin toss.
        let all = Workspaces {
            active: None,
            ..all
        };
        assert_eq!(all.workspace_to_restore(), Some(busier_id));

        // `active` pointing at a *detached* workspace must not resurrect it —
        // the user closed that window on purpose.
        let mut closed = workspace(vec![]);
        closed.open = false;
        let closed_id = closed.id;
        let mut open_one = workspace(vec![]);
        open_one.open = true;
        let open_id = open_one.id;
        let all = Workspaces {
            active: Some(closed_id),
            workspaces: vec![closed, open_one],
        };
        assert_eq!(all.workspace_to_restore(), Some(open_id));

        // Nothing open at all: launch has no workspace to come up on and shows
        // the home page instead of inventing one.
        let mut none_open = workspace(vec![]);
        none_open.open = false;
        let all = Workspaces {
            active: None,
            workspaces: vec![none_open],
        };
        assert_eq!(all.workspace_to_restore(), None);
    }
}

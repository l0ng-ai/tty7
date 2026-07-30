//! The client's workspace bookkeeping: the in-memory [`Session`] shape a
//! window is built from, and the persisted [`WindowView`] entries — pure view
//! state, because the layout itself lives in each machine's daemon-owned tree
//! (`core::machine`).
//!
//! [`Session`] / [`SessionTab`] / [`SessionPane`] mirror the live `Pane` tree
//! without GPUI types. They are **not persisted any more**: the window builder
//! consumes them, the tree hydration produces them, and the closed-tab stack
//! holds them, all in memory.
//!
//! [`WindowViews`] is the file — `~/.config/tty7/views.json`, alongside
//! `config.json`. All IO is best-effort: a missing/corrupt file just means "no
//! views to restore", and write failures are logged rather than fatal — the
//! app must never crash or stall over view bookkeeping.

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
    /// factored out: a tab always belongs to exactly one workspace, a
    /// workspace names exactly one machine in [`WindowView::host`], and a
    /// window shows exactly one workspace — mixing local and remote tabs in one
    /// window is the thing tty7 never does. So the fully-qualified group key
    /// is `(view.host_id(), tab.sidebar_group)`, with the host half
    /// stored once per workspace instead of once per tab. Two machines whose
    /// repos share a root path can only collide inside one window, which the
    /// model does not permit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_group: Option<std::path::PathBuf>,
    /// The tab's identity in the daemon's machine tree, when this session was
    /// derived *from* that tree — so a window rebuilt from it addresses the
    /// daemon's tabs rather than minting new ids and churning them. **Never
    /// persisted**: the tree is the authority on its own ids, and a stale one
    /// written to disk would collide with a tab the daemon has since reused it
    /// for. `None` (every other source) mints a fresh id.
    #[serde(skip)]
    pub tree_id: Option<crate::core::machine::TabId>,
}

/// One workspace's contents: the open tabs and which one was active.
///
/// This is the unit a single window displays — the in-memory shape a window
/// is built from and lowered into, never persisted (the machine's tree is the
/// layout's home).
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

impl std::str::FromStr for WorkspaceId {
    type Err = uuid::Error;

    /// The inverse of `Display`, for the places a workspace id crosses a
    /// string-keyed boundary (the control dialect's attach verbs, which
    /// predate the typed tree) and has to come back out as itself.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(WorkspaceId)
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
/// | [`Wsl`](RemoteTarget::Wsl) | A distribution installed on this computer, as `wsl.exe -l -q` names it | `wsl:<distro>` |
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
    /// A WSL distribution, named exactly as `wsl -d` takes it.
    ///
    /// **The one machine that is configured zero times**: it is reached by
    /// spawning `wsl.exe`, so there is no address, no credential and no host
    /// key to spell out anywhere. The picker
    /// (`ui::remote_connect::available_hosts`) therefore offers every
    /// distribution installed on this computer rather than reading a store.
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
    /// `views.json` with `Box.Local` still derives the same id as `box.local`.
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

    /// Whether this machine is reached over SSH.
    ///
    /// The question "Restart Server" asks, and the answer
    /// [`router::restart_server`](crate::daemon::router) already gives: it routes
    /// the action for SSH machines and refuses the other two. A `LocalStdio`
    /// machine is a child process per connection, so there is nothing there to
    /// stop and start; a WSL distribution's server is started by this client,
    /// which makes "stop it and reconnect" the whole of the verb and not
    /// something a routed action has to carry out. Asked here rather than
    /// re-spelled at each call site, so the UI that offers the verb and the
    /// router that carries it out cannot disagree about who has it.
    ///
    /// Spelled out variant by variant rather than as a `matches!` of the three
    /// that say yes: this gates an action that ends every session on a machine,
    /// and a new [`RemoteTarget`] must not inherit an answer to that by falling
    /// off the end of a pattern. The compiler asks instead.
    pub fn is_ssh(&self) -> bool {
        match self {
            RemoteTarget::Profile { .. }
            | RemoteTarget::Alias { .. }
            | RemoteTarget::Direct { .. } => true,
            RemoteTarget::Wsl { .. } | RemoteTarget::LocalStdio { .. } => false,
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
/// The `workspace` id is the **remote's**, minted once and then used as the
/// workspace's id in that machine's daemon-owned tree
/// ([`crate::core::machine`]). A client-side [`WindowView`] carrying one of
/// these is a *view*, not the record: the layout lives on the remote, which
/// owns it.
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

    /// The wire key for this workspace — the form the string-keyed control
    /// verbs (the attach family) and the `ControlEvent::Layout` events carry.
    pub fn store_key(&self) -> String {
        self.workspace.to_string()
    }
}

/// One workspace's **view state** on this client: which workspace (and on
/// which machine), where its window last was, whether it was on screen, and
/// when it was last focused. The layout itself lives in the machine's tree —
/// this entry is deliberately only what the tree cannot know, the facts about
/// *this client's windows*. Closing a window is a *detach*: the panes keep
/// running in the daemon and the entry stays here with `open: false`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowView {
    #[serde(default)]
    pub id: WorkspaceId,
    /// Geometry this workspace's window last occupied, so reopening it lands
    /// where the user left it rather than at the shared default. `None` for a
    /// workspace that has never been on screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<crate::core::window_state::WindowState>,
    /// Whether a window was showing this workspace at quit. Launch reopens
    /// exactly one of the `open` ones; the rest wait in the picker.
    #[serde(default)]
    pub open: bool,
    /// Unix seconds when this workspace was last focused, for "2 minutes ago"
    /// in the picker and for ordering it. 0 == never recorded.
    ///
    /// The machine's tree keeps its own recency; this copy exists because
    /// launch has to order entries before any tree has been pulled.
    #[serde(default)]
    pub last_active: u64,
    /// The machine this workspace's panes and files live on. `None` means this
    /// one. A `Some` entry keeps its own client-side `id` (the window
    /// registry's handle) while `host.workspace` names the workspace on that
    /// machine — see [`RemoteRef`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<RemoteRef>,
    /// What this workspace was *called* the last time its machine answered, and
    /// the path it was about — the picker's two lines.
    ///
    /// **A render hint, never an authority.** The machine's tree owns both (it
    /// derives them from the tabs' repo groups and its panes' cwds), and
    /// whenever the tree answers, the tree wins. This copy exists because the
    /// picker's whole job is choosing among machines that are *not* answering:
    /// a laptop that has been shut since Friday still has to be listed as
    /// "tty7 — ~/repo/tty7" rather than as "Untitled" with a blank subtitle,
    /// which is a row nobody can act on. Stamped on every save (and on the way
    /// out, when a window closes), so what is on file is the last thing the
    /// user actually saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The subject path behind [`label`](Self::label) — see there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

impl Default for WindowView {
    fn default() -> Self {
        Self {
            id: WorkspaceId::new(),
            window: None,
            open: true,
            last_active: now_secs(),
            host: None,
            label: None,
            subject: None,
        }
    }
}

impl WindowView {
    /// Stamp this workspace as just-focused.
    pub fn touch(&mut self) {
        self.last_active = now_secs();
    }

    // ----- the local / remote split ----------------------------------------

    /// A client-side entry for a workspace that lives on another machine.
    pub fn on_remote(host: RemoteRef) -> WindowView {
        WindowView {
            host: Some(host),
            ..WindowView::default()
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
}

/// The whole `views.json`: every workspace tty7 knows about, plus which one
/// had focus at quit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowViews {
    pub views: Vec<WindowView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<WorkspaceId>,
}

impl WindowViews {
    /// Load every saved view. Returns `None` when the file is absent or
    /// unreadable (normal first run), and `None` with a warning when it fails
    /// to parse — never panics.
    pub fn load() -> Option<Self> {
        let path = Self::path()?;
        let text = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str(crate::core::config::strip_bom(&text)) {
            Ok(loaded) => Some(loaded),
            Err(e) => {
                log::warn!("failed to parse views at {}: {e}; ignoring", path.display());
                None
            }
        }
    }

    pub fn get(&self, id: WorkspaceId) -> Option<&WindowView> {
        self.views.iter().find(|w| w.id == id)
    }

    pub fn get_mut(&mut self, id: WorkspaceId) -> Option<&mut WindowView> {
        self.views.iter_mut().find(|w| w.id == id)
    }

    /// The workspaces that had a window at the last quit, in their saved order.
    ///
    /// Note that launch does **not** restore all of these — see
    /// [`workspace_to_restore`](Self::workspace_to_restore). They are still the
    /// set that matters here, because every one of them is holding live daemon
    /// panes and none of them may be forgotten.
    pub fn open_views(&self) -> impl Iterator<Item = &WindowView> {
        self.views.iter().filter(|w| w.open)
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
            self.open_views()
                .max_by_key(|w| w.last_active)
                .map(|w| w.id)
        })
    }

    /// Persist as JSON, creating the parent directory if needed. Any
    /// IO/serialization error is logged and swallowed — the app must never
    /// crash or stall over view bookkeeping.
    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                log::warn!("failed to create views dir {}: {e}", parent.display());
                return;
            }
        }
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("failed to serialize views: {e}");
                return;
            }
        };
        if let Err(e) = crate::core::config::write_atomic(&path, json.as_bytes()) {
            log::warn!("failed to write views to {}: {e}", path.display());
        }
    }

    /// `~/.config/tty7/views.json`, alongside `config.json`.
    ///
    /// A fresh name, not `session.json`: that file's document embedded whole
    /// layouts, this one is pure view state, and the migration policy for the
    /// tree refactor is deliberately none — an old file is simply ignored.
    fn path() -> Option<PathBuf> {
        crate::core::config::config_path("views.json")
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

/// Helpers for every test that touches the on-disk `views.json`. The
/// config-dir pin is process-wide (`set_config_dir` is first-call-wins), so
/// the file is process-wide too — any test that reads or writes it must hold
/// [`lock_session_file`] across the whole read/write sequence, or parallel
/// tests clobber each other's file.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    static SESSION_FILE: Mutex<()> = Mutex::new(());

    /// Serialize access to the shared `views.json`.
    pub(crate) fn lock_session_file() -> MutexGuard<'static, ()> {
        // A poisoned lock just means another test failed mid-sequence; every
        // holder rewrites the file from scratch, so the state is still sound.
        SESSION_FILE.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Pin the process config dir at a shared temp location so `save`/`load`
    /// (which resolve `views.json` under it) never touch the real `~/.config`.
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

    fn view() -> WindowView {
        WindowView::default()
    }

    fn remote_view(alias: &str) -> WindowView {
        WindowView::on_remote(RemoteRef::new(
            RemoteTarget::Alias {
                alias: alias.into(),
            },
            WorkspaceId::new(),
        ))
    }

    #[test]
    fn views_round_trip_through_their_file() {
        let _file = lock_session_file();
        pin_config_dir();
        let mut entry = remote_view("build-box");
        entry.open = false;
        entry.last_active = 1_700_000_000;
        let id = entry.id;
        let host = entry.host.clone();
        WindowViews {
            active: Some(id),
            views: vec![entry],
        }
        .save();
        let loaded = WindowViews::load().expect("a saved views file should load back");
        let only = &loaded.views[0];
        assert_eq!(only.id, id, "identity must survive a restart");
        assert_eq!(
            only.host, host,
            "the remote pointer is the load-bearing half"
        );
        assert!(!only.open);
        assert_eq!(only.last_active, 1_700_000_000);
        assert_eq!(loaded.active, Some(id));
    }

    /// The migration policy for the tree refactor is deliberately none: an old
    /// `session.json` (whatever its shape) is not read, and a `views.json`
    /// missing every field still decodes rather than erroring a launch.
    #[test]
    fn an_empty_or_partial_file_decodes_to_defaults() {
        let empty: WindowViews = serde_json::from_str("{}").unwrap();
        assert!(empty.views.is_empty());
        assert!(empty.active.is_none());
        let partial: WindowViews = serde_json::from_str(r#"{"views":[{}]}"#).unwrap();
        assert_eq!(partial.views.len(), 1);
        assert!(!partial.views[0].is_remote());
    }

    // ── Remote references ───────────────────────────────────────────────────

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

    /// Which machines can be told to restart their server. The two that cannot
    /// are not an omission: their server is this client's own doing, so there is
    /// nothing on the far side to stop and start, and the router refuses the
    /// action for exactly the same reason. A new variant has to answer this
    /// question rather than inherit an answer.
    #[test]
    fn only_ssh_machines_have_a_server_to_restart() {
        assert!(
            RemoteTarget::Profile {
                id: uuid::Uuid::nil()
            }
            .is_ssh()
        );
        assert!(
            RemoteTarget::Alias {
                alias: "devbox".into()
            }
            .is_ssh()
        );
        assert!(RemoteTarget::direct("me", "box.local", 22).is_ssh());
        assert!(
            !RemoteTarget::Wsl {
                distro: "Ubuntu".into()
            }
            .is_ssh(),
            "a distribution's server is started by this client"
        );
        assert!(
            !RemoteTarget::LocalStdio {
                program: "tty7-server".into(),
                args: vec!["--stdio".into()],
            }
            .is_ssh(),
            "a stdio machine is a child process per connection"
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
    /// its key is distinct, its id is not [`HostId::LOCAL`](crate::host::HostId::LOCAL),
    /// and two different server binaries are two different machines.
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
    /// many workspaces — and never `HostId::LOCAL`.
    #[test]
    fn views_on_one_box_share_a_host_id() {
        let target = RemoteTarget::Alias {
            alias: "devbox".into(),
        };
        let a = WindowView::on_remote(RemoteRef::new(target.clone(), WorkspaceId::new()));
        let b = WindowView::on_remote(RemoteRef::new(target.clone(), WorkspaceId::new()));
        assert_ne!(
            a.host.as_ref().unwrap().workspace,
            b.host.as_ref().unwrap().workspace
        );
        assert_eq!(a.host_id(), b.host_id(), "same machine, one HostId");
        assert!(!a.host_id().is_local());

        // A different machine is a different id.
        let other = remote_view("other");
        assert_ne!(a.host_id(), other.host_id());

        // And the local shape answers LOCAL, with nothing derived.
        assert_eq!(view().host_id(), crate::host::HostId::LOCAL);
        assert_eq!(
            a.host.as_ref().unwrap().store_key(),
            a.host.as_ref().unwrap().workspace.to_string()
        );
    }

    // ── Launch ──────────────────────────────────────────────────────────────

    #[test]
    fn open_views_partition_by_flag() {
        let mut open_one = view();
        open_one.open = true;
        let mut closed = view();
        closed.open = false;
        let open_id = open_one.id;
        let all = WindowViews {
            active: None,
            views: vec![open_one, closed],
        };
        assert_eq!(
            all.open_views().map(|w| w.id).collect::<Vec<_>>(),
            vec![open_id]
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
        let mut focused = view();
        focused.open = true;
        focused.last_active = 100;
        let mut busier = view();
        busier.open = true;
        busier.last_active = 900;
        let (focused_id, busier_id) = (focused.id, busier.id);

        let all = WindowViews {
            active: Some(focused_id),
            views: vec![focused, busier],
        };
        assert_eq!(all.workspace_to_restore(), Some(focused_id));
        assert_eq!(
            all.open_views().count(),
            2,
            "the others stay open in the store — launch detaches them, this does not"
        );

        // No focus recorded (or it named a workspace that was closed first):
        // recency is the fallback, not a coin toss.
        let all = WindowViews {
            active: None,
            ..all
        };
        assert_eq!(all.workspace_to_restore(), Some(busier_id));

        // `active` pointing at a *detached* workspace must not resurrect it —
        // the user closed that window on purpose.
        let mut closed = view();
        closed.open = false;
        let closed_id = closed.id;
        let mut open_one = view();
        open_one.open = true;
        let open_id = open_one.id;
        let all = WindowViews {
            active: Some(closed_id),
            views: vec![closed, open_one],
        };
        assert_eq!(all.workspace_to_restore(), Some(open_id));

        // Nothing open at all: launch has no workspace to come up on and shows
        // the home page instead of inventing one.
        let mut none_open = view();
        none_open.open = false;
        let all = WindowViews {
            active: None,
            views: vec![none_open],
        };
        assert_eq!(all.workspace_to_restore(), None);
    }
}

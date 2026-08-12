use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const MAX_FRAME: usize = 64 * 1024 * 1024;

pub const PROTOCOL_VERSION: u32 = 5;

pub const FEATURE_PANE_OWNER: &str = "pane-owner";

/// The daemon echoes a `DaemonMsg::Size` to the controlling subscriber, in
/// stream order, when it applies a `ClientMsg::Resize`. A client that sees
/// this feature defers its local grid reflow to that echo so the reflow lands
/// at the stream position where the PTY actually changed geometry; against an
/// older daemon it must keep reflowing locally at request time.
pub const FEATURE_RESIZE_ECHO: &str = "resize-echo";

/// The daemon can seed a new pane with the screen a dead one left behind, named
/// by `ClientMsg::Spawn`'s `restore` field. A client that does not see this
/// feature leaves the field out: an older daemon would ignore it and spawn a
/// blank pane, which is the same outcome, but sending it would make the wire
/// claim a restore that never happened.
pub const FEATURE_RESTORE_SCROLLBACK: &str = "restore-scrollback";

/// The daemon can replace its own binary without stopping, keeping every pty
/// and everything running on one — `ClientMsg::Handoff`. Advertised only where
/// it can actually be done, which is where `execve` exists, so a client can use
/// it to choose between offering an upgrade that costs the user nothing and one
/// that costs them every running command.
pub const FEATURE_HANDOFF: &str = "handoff";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonVersion {
    pub protocol: u32,
    #[serde(default)]
    pub build: String,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub instance: String,
}

impl DaemonVersion {
    pub fn current() -> DaemonVersion {
        let mut features = vec![
            FEATURE_PANE_OWNER.to_string(),
            FEATURE_RESIZE_ECHO.to_string(),
            FEATURE_RESTORE_SCROLLBACK.to_string(),
        ];
        if cfg!(unix) {
            features.push(FEATURE_HANDOFF.to_string());
        }
        DaemonVersion {
            protocol: PROTOCOL_VERSION,
            build: env!("CARGO_PKG_VERSION").to_string(),
            features,
            instance: process_instance().to_string(),
        }
    }

    pub fn has_feature(&self, name: &str) -> bool {
        self.features.iter().any(|f| f == name)
    }
}

pub fn process_instance() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u16,
    pub cell_h: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub args_are_tty7_defaults: bool,
}

pub fn ssh_option_takes_value(flag: char) -> bool {
    matches!(
        flag,
        'B' | 'b'
            | 'c'
            | 'D'
            | 'E'
            | 'e'
            | 'F'
            | 'I'
            | 'i'
            | 'J'
            | 'L'
            | 'l'
            | 'm'
            | 'O'
            | 'o'
            | 'p'
            | 'Q'
            | 'R'
            | 'S'
            | 'W'
            | 'w'
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: u64,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub title: String,
    pub alive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteContext {
    pub kind: RemoteKind,
    pub argv: Vec<String>,
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteKind {
    Ssh,
    NativeSsh,
    Wsl,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopbackForwardRequest {
    pub pane_id: u64,
    pub remote_host: String,
    pub remote_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRequest {
    pub workspace: crate::core::session::WorkspaceId,
    pub spec: Box<NativeSshSpec>,
    pub view_pane: u64,
    pub op: WorkspaceOp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceOp {
    EnsureLoopback {
        remote_host: String,
        remote_port: u16,
    },
    AddForward {
        rule: SshForwardRule,
    },
    RemoveForward {
        forward_id: u64,
    },
    ListForwards,
    TeardownForwards,
    SftpList {
        path: String,
    },
    SftpOp {
        op: SftpOp,
    },
    SftpTransferStart {
        spec: SftpTransferSpec,
    },
    SftpTransferList,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopbackForward {
    pub local_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LoopbackForwardId {
    pub pane_id: u64,
    pub target: String,
    pub remote_host: String,
    pub remote_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopbackForwardInfo {
    pub id: LoopbackForwardId,
    pub local_port: u16,
    pub age_secs: u64,
    pub idle_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SshAuthMode {
    #[default]
    Auto,
    Gssapi,
    Password,
    PublicKey,
    Agent,
    KeyboardInteractive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SshProxy {
    #[default]
    None,
    Command(String),
    Socks {
        host: String,
        port: u16,
    },
    Http {
        host: String,
        port: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SshAlgorithms {
    #[serde(default)]
    pub kex: Vec<String>,
    #[serde(default)]
    pub cipher: Vec<String>,
    #[serde(default)]
    pub mac: Vec<String>,
    #[serde(default)]
    pub host_key: Vec<String>,
    #[serde(default)]
    pub compression: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SshForwardKind {
    Local,
    Remote,
    Dynamic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshForwardRule {
    pub kind: SshForwardKind,
    pub bind_host: String,
    pub bind_port: u16,
    #[serde(default)]
    pub target_host: String,
    #[serde(default)]
    pub target_port: u16,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SftpEntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SftpEntry {
    pub name: String,
    pub kind: SftpEntryKind,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime: u64,
    #[serde(default)]
    pub permissions: u32,
    #[serde(default)]
    pub target_is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SftpOp {
    Stat { path: String },
    Mkdir { path: String },
    CreateFile { path: String },
    RemoveFile { path: String },
    RemoveDir { path: String },
    Rename { from: String, to: String },
    Chmod { path: String, mode: u32 },
    Readlink { path: String },
    Realpath { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SftpOpResult {
    Done,
    Stat(SftpEntry),
    Link(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SftpTransferKind {
    Upload,
    Download,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SftpTransferSpec {
    pub pane_id: u64,
    pub kind: SftpTransferKind,
    pub local: PathBuf,
    pub remote: String,
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SftpJobState {
    Running,
    Done,
    Error,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SftpJobProgress {
    pub job_id: u64,
    pub pane_id: u64,
    pub kind: SftpTransferKind,
    pub state: SftpJobState,
    #[serde(default)]
    pub current: String,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default)]
    pub bytes_total: u64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub local: String,
    #[serde(default)]
    pub remote: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForwardStatus {
    Listening,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedForward {
    pub id: u64,
    pub pane_id: u64,
    pub kind: SshForwardKind,
    pub bind_host: String,
    pub bind_port: u16,
    #[serde(default)]
    pub target_host: String,
    #[serde(default)]
    pub target_port: u16,
    #[serde(default)]
    pub description: Option<String>,
    pub status: ForwardStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcEntry {
    pub pid: u32,
    pub name: String,
    pub depth: u8,
    #[serde(default)]
    pub foreground: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortEntry {
    pub port: u16,
    pub pid: u32,
    pub name: String,
    /// The address the socket is bound to, as `lsof` spells it — `*`,
    /// `0.0.0.0`, `127.0.0.1`, `[::1]`, or a specific interface.
    ///
    /// `serde(default)` because a daemon from before this field existed
    /// answers `QueryProcs` without it, and an empty address is read the same
    /// way that daemon's callers read every address: as localhost.
    #[serde(default)]
    pub addr: String,
}

impl PortEntry {
    /// Whether a bound address can be reached on this machine's loopback —
    /// true for the wildcards and the loopback addresses themselves, false for
    /// a socket pinned to one specific non-loopback interface.
    pub fn reaches_loopback(addr: &str) -> bool {
        matches!(
            addr,
            "" | "*" | "0.0.0.0" | "::" | "[::]" | "127.0.0.1" | "::1" | "[::1]" | "localhost"
        )
    }

    /// What to copy, and what to open in a browser: `host:port`.
    ///
    /// Loopback and wildcard binds are spelled `localhost`, which is what
    /// anyone typing the address by hand would write. A socket bound to one
    /// specific interface keeps that interface — the panel presents this as
    /// the address of the pane's server, and `localhost` would not be it.
    pub fn authority(&self) -> String {
        match Self::reaches_loopback(&self.addr) {
            true => format!("localhost:{}", self.port),
            false => format!("{}:{}", self.addr, self.port),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneProcs {
    pub procs: Vec<ProcEntry>,
    pub ports: Vec<PortEntry>,
}

fn default_term() -> String {
    "xterm-256color".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeSshSpec {
    pub host: String,
    pub port: u16,
    pub user: String,

    pub auth_mode: SshAuthMode,
    #[serde(default)]
    pub identity_files: Vec<String>,
    #[serde(default)]
    pub agent_forward: bool,

    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub key_passphrases: Option<std::collections::HashMap<String, String>>,

    #[serde(default)]
    pub proxy: SshProxy,
    #[serde(default)]
    pub jump: Option<Box<NativeSshSpec>>,

    #[serde(default)]
    pub forwards: Vec<SshForwardRule>,

    #[serde(default)]
    pub keepalive_interval_s: Option<u32>,
    #[serde(default)]
    pub keepalive_count_max: Option<u32>,
    #[serde(default)]
    pub connect_timeout_s: Option<u32>,

    #[serde(default)]
    pub algorithms: SshAlgorithms,
    #[serde(default)]
    pub x11: bool,

    #[serde(default = "default_term")]
    pub term: String,
    #[serde(default = "default_true")]
    pub verify_host_keys: bool,
    #[serde(default)]
    pub skip_banner: bool,
    #[serde(default = "default_true")]
    pub shell_integration: bool,
    #[serde(default)]
    pub login_script: Vec<String>,

    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub profile_id: Option<String>,
}

impl NativeSshSpec {
    #[allow(dead_code)]
    pub fn without_secrets(&self) -> NativeSshSpec {
        NativeSshSpec {
            password: None,
            key_passphrases: None,
            jump: self.jump.as_ref().map(|j| Box::new(j.without_secrets())),
            ..self.clone()
        }
    }
}

impl std::fmt::Debug for NativeSshSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSshSpec")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("auth_mode", &self.auth_mode)
            .field("identity_files", &self.identity_files)
            .field("agent_forward", &self.agent_forward)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field(
                "key_passphrases",
                &self.key_passphrases.as_ref().map(|m| m.len()),
            )
            .field("proxy", &self.proxy)
            .field("jump", &self.jump)
            .field("forwards", &self.forwards)
            .field("keepalive_interval_s", &self.keepalive_interval_s)
            .field("keepalive_count_max", &self.keepalive_count_max)
            .field("connect_timeout_s", &self.connect_timeout_s)
            .field("algorithms", &self.algorithms)
            .field("x11", &self.x11)
            .field("term", &self.term)
            .field("verify_host_keys", &self.verify_host_keys)
            .field("skip_banner", &self.skip_banner)
            .field("shell_integration", &self.shell_integration)
            .field("login_script", &self.login_script)
            .field("display_name", &self.display_name)
            .field("profile_id", &self.profile_id)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownHostEntry {
    pub host: String,
    #[serde(default)]
    pub marker: Option<String>,
    pub key_type: String,
    pub fingerprint_sha256: String,
    pub id: KnownHostId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownHostId {
    pub host: String,
    pub key_type: String,
    pub keyblob: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KiPrompt {
    pub text: String,
    pub echo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthPromptKind {
    Password {
        user: String,
        host: String,
    },
    KeyPassphrase {
        key_path: String,
        comment: String,
        /// The connection already had a passphrase for this key and the key
        /// would not decrypt with it, so the one the user is about to type
        /// replaces a stored secret rather than adding a first one. Only the
        /// daemon can know this — the client sees a passphrase prompt and
        /// cannot tell a first ask from a second — and without it a wrong
        /// "remember" bricks the key for every later connection.
        ///
        /// `#[serde(default)]`, and deliberately not a `PROTOCOL_VERSION`
        /// bump: an older peer on either side of this field simply never sets
        /// it, and serde ignores fields it does not know, so the flag is
        /// compatible in both directions across daemon↔GUI and GUI↔remote
        /// `tty7-server`.
        #[serde(default)]
        rejected: bool,
    },
    KeyboardInteractive {
        name: String,
        instructions: String,
        prompts: Vec<KiPrompt>,
        /// The round that just failed was answered with the stored password
        /// rather than by the user, so this prompt exists to replace it. Same
        /// compatibility reasoning as `KeyPassphrase::rejected` above.
        #[serde(default)]
        stored_rejected: bool,
    },
    HostKeyUnknown {
        host: String,
        port: u16,
        algorithm: String,
        fingerprint_sha256: String,
        /// The algorithm this host is already on file under, when the key is
        /// new only because its algorithm is.
        ///
        /// A field rather than a variant of its own on purpose. This enum is
        /// externally tagged and travels both to the GUI and to the remote
        /// `tty7-server`, either of which may be an older build; an unknown
        /// variant fails the whole decode, while an unknown field is ignored
        /// and a missing one defaults. So an old peer shows the plain
        /// unknown-host confirmation, which is the right prompt either way.
        #[serde(default)]
        previously_known_as: Option<String>,
    },
    HostKeyChanged {
        host: String,
        port: u16,
        algorithm: String,
        fingerprint_sha256: String,
        old_fingerprint_sha256: String,
    },
    Banner {
        text: String,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthResponse {
    Secret(String),
    Secrets(Vec<String>),
    HostKeyDecision { accept: bool, remember: bool },
    Cancelled,
}

impl std::fmt::Debug for AuthResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthResponse::Secret(_) => f.write_str("Secret(<redacted>)"),
            AuthResponse::Secrets(v) => write!(f, "Secrets(<{} redacted>)", v.len()),
            AuthResponse::HostKeyDecision { accept, remember } => f
                .debug_struct("HostKeyDecision")
                .field("accept", accept)
                .field("remember", remember)
                .finish(),
            AuthResponse::Cancelled => f.write_str("Cancelled"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SshPhase {
    Connecting,
    Authenticating,
    Connected,
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    Spawn {
        cwd: Option<PathBuf>,
        size: WinSize,
        shell: Option<ShellSpec>,
        owner: Option<String>,
        workspace: Option<String>,
        restore: Option<RestoreFrom>,
    },
    Attach {
        pane_id: u64,
        size: WinSize,
    },
    Observe {
        pane_id: u64,
        size: WinSize,
    },
    Input(Vec<u8>),
    SendInput {
        pane_id: u64,
        bytes: Vec<u8>,
    },
    Resize(WinSize),
    Detach,
    Kill {
        pane_id: u64,
    },
    List,
    Shutdown,
    /// Become `exe` without stopping: the daemon rewrites itself in place and
    /// keeps every pty, shell and pane id it is holding. The connection dies in
    /// the process — the new image has never heard of it — so this is the last
    /// thing a client can say on it, and the reply is the socket closing.
    ///
    /// Unix only. Elsewhere the daemon answers with an error and the caller
    /// falls back to stopping and starting it.
    Handoff {
        exe: PathBuf,
    },
    EnsureLoopbackForward(LoopbackForwardRequest),
    ListLoopbackForwards,
    CloseLoopbackForward(LoopbackForwardId),
    SpawnNativeSsh {
        cwd: Option<PathBuf>,
        size: WinSize,
        spec: Box<NativeSshSpec>,
    },
    AuthResponse {
        request_id: u64,
        response: AuthResponse,
    },
    ListKnownHosts,
    DeleteKnownHost(KnownHostId),
    SftpList {
        pane_id: u64,
        path: String,
    },
    SftpOp {
        pane_id: u64,
        op: SftpOp,
    },
    SftpTransferStart(SftpTransferSpec),
    SftpTransferCancel {
        job_id: u64,
    },
    SftpTransferList {
        pane_id: u64,
    },
    AddForward {
        pane_id: u64,
        rule: SshForwardRule,
    },
    RemoveForward {
        pane_id: u64,
        forward_id: u64,
    },
    ListForwards {
        pane_id: u64,
    },
    QueryProcs {
        pane_id: u64,
    },
    OnWorkspace(Box<WorkspaceRequest>),
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonMsg {
    Spawned {
        pane_id: u64,
    },
    Size(WinSize),
    Snapshot(Vec<u8>),
    Output(Vec<u8>),
    /// A kitty graphics image lifted out of the PTY stream daemon-side (issue
    /// #213). Carried out-of-band as a compact binary frame
    /// ([`crate::core::kitty_graphics::Image::encode_frame`]) so the base64 text
    /// never rides the socket and the client's VT parser never sees it. The
    /// pixel payload stays *compressed* on the wire — the client inflates — so a
    /// remote pane's frames don't balloon across the SSH tunnel.
    Image(Vec<u8>),
    /// A kitty graphics delete (`a=d`) lifted out of the PTY stream daemon-side.
    /// Payload is a compact selector frame
    /// ([`crate::core::kitty_graphics::ImageDelete::encode`]) telling the client
    /// which stored image(s)/placement(s) to drop.
    DeleteImage(Vec<u8>),
    Cwd(PathBuf),
    Prompt {
        active: bool,
        at_prompt: bool,
        last_exit: Option<i32>,
    },
    Exited {
        code: Option<i32>,
    },
    PaneList(Vec<PaneInfo>),
    InputAck {
        pane_id: u64,
    },
    RemoteContext(Option<RemoteContext>),
    Agent(Option<crate::core::cli_agent::CLIAgent>),
    AgentStatus(Option<crate::core::cli_agent::AgentSessionState>),
    LoopbackForward(LoopbackForward),
    LoopbackForwardList(Vec<LoopbackForwardInfo>),
    AuthPrompt {
        request_id: u64,
        prompt: AuthPromptKind,
    },
    SshStatus {
        phase: SshPhase,
    },
    KnownHostsList(Vec<KnownHostEntry>),
    SftpEntries(Vec<SftpEntry>),
    SftpOpResult(SftpOpResult),
    SftpTransferStarted {
        job_id: u64,
    },
    SftpTransferProgress(Vec<SftpJobProgress>),
    ForwardList(Vec<ManagedForward>),
    Procs(PaneProcs),
    Version(DaemonVersion),
    Error(String),
}

mod kind {
    pub const SPAWN: u8 = 1;
    pub const ATTACH: u8 = 2;
    pub const INPUT: u8 = 3;
    pub const RESIZE: u8 = 4;
    pub const DETACH: u8 = 5;
    pub const KILL: u8 = 6;
    pub const LIST: u8 = 7;
    pub const SHUTDOWN: u8 = 8;
    pub const SPAWN_SHELL: u8 = 9;
    pub const ENSURE_LOOPBACK_FORWARD: u8 = 10;
    pub const LIST_LOOPBACK_FORWARDS: u8 = 11;
    pub const CLOSE_LOOPBACK_FORWARD: u8 = 12;
    pub const SPAWN_NATIVE_SSH: u8 = 14;
    pub const AUTH_RESPONSE: u8 = 15;
    pub const LIST_KNOWN_HOSTS: u8 = 16;
    pub const DELETE_KNOWN_HOST: u8 = 17;
    pub const SFTP_LIST: u8 = 30;
    pub const SFTP_OP: u8 = 31;
    pub const SFTP_TRANSFER_START: u8 = 32;
    pub const SFTP_TRANSFER_CANCEL: u8 = 33;
    pub const SFTP_TRANSFER_LIST: u8 = 34;
    pub const ADD_FORWARD: u8 = 20;
    pub const REMOVE_FORWARD: u8 = 21;
    pub const LIST_FORWARDS: u8 = 22;
    pub const VERSION: u8 = 40;
    pub const QUERY_PROCS: u8 = 50;
    pub const ON_WORKSPACE: u8 = 52;
    pub const SPAWN_OWNED: u8 = 53;
    pub const OBSERVE: u8 = 54;
    pub const SEND_INPUT: u8 = 55;
    pub const HANDOFF: u8 = 56;

    pub const SPAWNED: u8 = 1;
    pub const SNAPSHOT: u8 = 2;
    pub const OUTPUT: u8 = 3;
    pub const CWD: u8 = 4;
    pub const PROMPT: u8 = 5;
    pub const EXITED: u8 = 6;
    pub const PANE_LIST: u8 = 7;
    pub const ERROR: u8 = 8;
    pub const SIZE: u8 = 9;
    pub const REMOTE_CONTEXT: u8 = 10;
    pub const LOOPBACK_FORWARD: u8 = 11;
    pub const LOOPBACK_FORWARD_LIST: u8 = 12;
    pub const AUTH_PROMPT: u8 = 13;
    pub const SSH_STATUS: u8 = 14;
    pub const KNOWN_HOSTS_LIST: u8 = 15;
    pub const SFTP_ENTRIES: u8 = 30;
    pub const SFTP_OP_RESULT: u8 = 31;
    pub const SFTP_TRANSFER_STARTED: u8 = 32;
    pub const SFTP_TRANSFER_PROGRESS: u8 = 33;
    pub const FORWARD_LIST: u8 = 20;
    pub const AGENT: u8 = 21;
    pub const AGENT_STATUS: u8 = 22;
    pub const VERSION_REPLY: u8 = 40;
    pub const PROCS: u8 = 50;
    pub const INPUT_ACK: u8 = 51;
    /// `Image` — a kitty graphics frame lifted out of the PTY stream (issue
    /// #213). 60 sits clear of every range above; the payload is the compact
    /// binary encoding, not JSON, so it stays outside the `to_json` arms.
    pub const IMAGE: u8 = 60;
    /// `DeleteImage` — a kitty graphics `a=d` delete lifted out of the stream
    /// (issue #213). Compact binary selector, like `IMAGE`.
    pub const DELETE_IMAGE: u8 = 61;
}

pub fn write_frame<W: Write>(w: &mut W, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = payload.len();
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame payload exceeds MAX_FRAME",
        ));
    }
    w.write_all(&(len as u32).to_le_bytes())?;
    w.write_all(&[kind])?;
    w.write_all(payload)?;
    Ok(())
}

pub fn read_frame<R: Read>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame payload exceeds MAX_FRAME",
        ));
    }
    let mut kind = [0u8; 1];
    r.read_exact(&mut kind)?;
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((kind[0], payload))
}

pub fn peek_frame_kind(buf: &[u8]) -> Option<u8> {
    (buf.len() >= 5).then(|| buf[4])
}

pub fn is_error_kind(kind: u8) -> bool {
    kind == kind::ERROR
}

pub fn take_frame(buf: &mut Vec<u8>) -> io::Result<Option<(u8, Vec<u8>)>> {
    const HEADER: usize = 5;
    if buf.len() < HEADER {
        return Ok(None);
    }
    let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame payload exceeds MAX_FRAME",
        ));
    }
    if buf.len() < HEADER + len {
        return Ok(None);
    }
    let kind = buf[4];
    let payload = buf[HEADER..HEADER + len].to_vec();
    buf.drain(..HEADER + len);
    Ok(Some((kind, payload)))
}

fn to_json<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn from_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> io::Result<T> {
    serde_json::from_slice(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OwnedSpawn {
    #[serde(default)]
    cwd: Option<PathBuf>,
    size: WinSize,
    #[serde(default)]
    shell: Option<ShellSpec>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    restore: Option<RestoreFrom>,
}

/// "This pane replaces one that died with the daemon."
///
/// Carried on a spawn rather than an attach because there is nothing to attach
/// to: the process is gone. The daemon looks up what pane `pane_id` last had on
/// its screen and seeds the new pane's ring with it, so the window shows the
/// output it lost under a shell that is plainly new.
///
/// `banner` is the line drawn between the two, and it comes from the client
/// because the daemon has no locale — it serves a GUI that might be running in
/// any language, and a CLI whose output is always English. A client that has
/// nothing to say can leave it out; the reset sequence is emitted either way.
///
/// Old daemons decode this frame without the field and simply spawn a blank
/// pane, which is what they did before it existed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreFrom {
    pub pane_id: u64,
    #[serde(default)]
    pub banner: Option<String>,
}

impl ClientMsg {
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        match self {
            ClientMsg::Spawn {
                cwd,
                size,
                shell: None,
                owner: None,
                workspace: None,
                restore: None,
            } => write_frame(w, kind::SPAWN, &to_json(&(cwd, size))?),
            ClientMsg::Spawn {
                cwd,
                size,
                shell: shell @ Some(_),
                owner: None,
                workspace: None,
                restore: None,
            } => write_frame(w, kind::SPAWN_SHELL, &to_json(&(cwd, size, shell))?),
            ClientMsg::Spawn {
                cwd,
                size,
                shell,
                owner,
                workspace,
                restore,
            } => write_frame(
                w,
                kind::SPAWN_OWNED,
                &to_json(&OwnedSpawn {
                    cwd: cwd.clone(),
                    size: *size,
                    shell: shell.clone(),
                    owner: owner.clone(),
                    workspace: workspace.clone(),
                    restore: restore.clone(),
                })?,
            ),
            ClientMsg::Attach { pane_id, size } => {
                write_frame(w, kind::ATTACH, &to_json(&(pane_id, size))?)
            }
            ClientMsg::Observe { pane_id, size } => {
                write_frame(w, kind::OBSERVE, &to_json(&(pane_id, size))?)
            }
            ClientMsg::Input(bytes) => write_frame(w, kind::INPUT, bytes),
            ClientMsg::SendInput { pane_id, bytes } => {
                write_frame(w, kind::SEND_INPUT, &to_json(&(pane_id, bytes))?)
            }
            ClientMsg::Resize(size) => write_frame(w, kind::RESIZE, &to_json(size)?),
            ClientMsg::Detach => write_frame(w, kind::DETACH, &[]),
            ClientMsg::Kill { pane_id } => write_frame(w, kind::KILL, &to_json(pane_id)?),
            ClientMsg::List => write_frame(w, kind::LIST, &[]),
            ClientMsg::Shutdown => write_frame(w, kind::SHUTDOWN, &[]),
            ClientMsg::Handoff { exe } => write_frame(w, kind::HANDOFF, &to_json(exe)?),
            ClientMsg::EnsureLoopbackForward(req) => {
                write_frame(w, kind::ENSURE_LOOPBACK_FORWARD, &to_json(req)?)
            }
            ClientMsg::ListLoopbackForwards => write_frame(w, kind::LIST_LOOPBACK_FORWARDS, &[]),
            ClientMsg::CloseLoopbackForward(id) => {
                write_frame(w, kind::CLOSE_LOOPBACK_FORWARD, &to_json(id)?)
            }
            ClientMsg::SpawnNativeSsh { cwd, size, spec } => {
                write_frame(w, kind::SPAWN_NATIVE_SSH, &to_json(&(cwd, size, spec))?)
            }
            ClientMsg::AuthResponse {
                request_id,
                response,
            } => write_frame(w, kind::AUTH_RESPONSE, &to_json(&(request_id, response))?),
            ClientMsg::ListKnownHosts => write_frame(w, kind::LIST_KNOWN_HOSTS, &[]),
            ClientMsg::DeleteKnownHost(id) => {
                write_frame(w, kind::DELETE_KNOWN_HOST, &to_json(id)?)
            }
            ClientMsg::SftpList { pane_id, path } => {
                write_frame(w, kind::SFTP_LIST, &to_json(&(pane_id, path))?)
            }
            ClientMsg::SftpOp { pane_id, op } => {
                write_frame(w, kind::SFTP_OP, &to_json(&(pane_id, op))?)
            }
            ClientMsg::SftpTransferStart(spec) => {
                write_frame(w, kind::SFTP_TRANSFER_START, &to_json(spec)?)
            }
            ClientMsg::SftpTransferCancel { job_id } => {
                write_frame(w, kind::SFTP_TRANSFER_CANCEL, &to_json(job_id)?)
            }
            ClientMsg::SftpTransferList { pane_id } => {
                write_frame(w, kind::SFTP_TRANSFER_LIST, &to_json(pane_id)?)
            }
            ClientMsg::AddForward { pane_id, rule } => {
                write_frame(w, kind::ADD_FORWARD, &to_json(&(pane_id, rule))?)
            }
            ClientMsg::RemoveForward {
                pane_id,
                forward_id,
            } => write_frame(w, kind::REMOVE_FORWARD, &to_json(&(pane_id, forward_id))?),
            ClientMsg::QueryProcs { pane_id } => {
                write_frame(w, kind::QUERY_PROCS, &to_json(pane_id)?)
            }
            ClientMsg::ListForwards { pane_id } => {
                write_frame(w, kind::LIST_FORWARDS, &to_json(pane_id)?)
            }
            ClientMsg::OnWorkspace(req) => write_frame(w, kind::ON_WORKSPACE, &to_json(req)?),
            ClientMsg::Version => write_frame(w, kind::VERSION, &[]),
        }
    }

    pub fn from_frame(k: u8, payload: Vec<u8>) -> io::Result<Self> {
        Ok(match k {
            kind::SPAWN => {
                let (cwd, size) = from_json(&payload)?;
                ClientMsg::Spawn {
                    cwd,
                    size,
                    shell: None,
                    owner: None,
                    workspace: None,
                    restore: None,
                }
            }
            kind::SPAWN_SHELL => {
                let (cwd, size, shell) = from_json(&payload)?;
                ClientMsg::Spawn {
                    cwd,
                    size,
                    shell,
                    owner: None,
                    workspace: None,
                    restore: None,
                }
            }
            kind::SPAWN_OWNED => {
                let OwnedSpawn {
                    cwd,
                    size,
                    shell,
                    owner,
                    workspace,
                    restore,
                } = from_json(&payload)?;
                ClientMsg::Spawn {
                    cwd,
                    size,
                    shell,
                    owner,
                    workspace,
                    restore,
                }
            }
            kind::ATTACH => {
                let (pane_id, size) = from_json(&payload)?;
                ClientMsg::Attach { pane_id, size }
            }
            kind::OBSERVE => {
                let (pane_id, size) = from_json(&payload)?;
                ClientMsg::Observe { pane_id, size }
            }
            kind::INPUT => ClientMsg::Input(payload),
            kind::SEND_INPUT => {
                let (pane_id, bytes) = from_json(&payload)?;
                ClientMsg::SendInput { pane_id, bytes }
            }
            kind::RESIZE => ClientMsg::Resize(from_json(&payload)?),
            kind::DETACH => ClientMsg::Detach,
            kind::KILL => ClientMsg::Kill {
                pane_id: from_json(&payload)?,
            },
            kind::LIST => ClientMsg::List,
            kind::SHUTDOWN => ClientMsg::Shutdown,
            kind::HANDOFF => ClientMsg::Handoff {
                exe: from_json(&payload)?,
            },
            kind::ENSURE_LOOPBACK_FORWARD => ClientMsg::EnsureLoopbackForward(from_json(&payload)?),
            kind::LIST_LOOPBACK_FORWARDS => ClientMsg::ListLoopbackForwards,
            kind::CLOSE_LOOPBACK_FORWARD => ClientMsg::CloseLoopbackForward(from_json(&payload)?),
            kind::SPAWN_NATIVE_SSH => {
                let (cwd, size, spec) = from_json(&payload)?;
                ClientMsg::SpawnNativeSsh { cwd, size, spec }
            }
            kind::AUTH_RESPONSE => {
                let (request_id, response) = from_json(&payload)?;
                ClientMsg::AuthResponse {
                    request_id,
                    response,
                }
            }
            kind::LIST_KNOWN_HOSTS => ClientMsg::ListKnownHosts,
            kind::DELETE_KNOWN_HOST => ClientMsg::DeleteKnownHost(from_json(&payload)?),
            kind::SFTP_LIST => {
                let (pane_id, path) = from_json(&payload)?;
                ClientMsg::SftpList { pane_id, path }
            }
            kind::SFTP_OP => {
                let (pane_id, op) = from_json(&payload)?;
                ClientMsg::SftpOp { pane_id, op }
            }
            kind::SFTP_TRANSFER_START => ClientMsg::SftpTransferStart(from_json(&payload)?),
            kind::SFTP_TRANSFER_CANCEL => ClientMsg::SftpTransferCancel {
                job_id: from_json(&payload)?,
            },
            kind::SFTP_TRANSFER_LIST => ClientMsg::SftpTransferList {
                pane_id: from_json(&payload)?,
            },
            kind::QUERY_PROCS => ClientMsg::QueryProcs {
                pane_id: from_json(&payload)?,
            },
            kind::ADD_FORWARD => {
                let (pane_id, rule) = from_json(&payload)?;
                ClientMsg::AddForward { pane_id, rule }
            }
            kind::REMOVE_FORWARD => {
                let (pane_id, forward_id) = from_json(&payload)?;
                ClientMsg::RemoveForward {
                    pane_id,
                    forward_id,
                }
            }
            kind::LIST_FORWARDS => ClientMsg::ListForwards {
                pane_id: from_json(&payload)?,
            },
            kind::ON_WORKSPACE => ClientMsg::OnWorkspace(from_json(&payload)?),
            kind::VERSION => ClientMsg::Version,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown ClientMsg kind {other}"),
                ));
            }
        })
    }

    pub fn read<R: Read>(r: &mut R) -> io::Result<Self> {
        let (k, payload) = read_frame(r)?;
        Self::from_frame(k, payload)
    }
}

impl DaemonMsg {
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        match self {
            DaemonMsg::Spawned { pane_id } => write_frame(w, kind::SPAWNED, &to_json(pane_id)?),
            DaemonMsg::Size(size) => write_frame(w, kind::SIZE, &to_json(size)?),
            DaemonMsg::Snapshot(bytes) => write_frame(w, kind::SNAPSHOT, bytes),
            DaemonMsg::Output(bytes) => write_frame(w, kind::OUTPUT, bytes),
            DaemonMsg::Image(frame) => write_frame(w, kind::IMAGE, frame),
            DaemonMsg::DeleteImage(sel) => write_frame(w, kind::DELETE_IMAGE, sel),
            DaemonMsg::Cwd(path) => write_frame(w, kind::CWD, &to_json(path)?),
            DaemonMsg::Prompt {
                active,
                at_prompt,
                last_exit,
            } => write_frame(w, kind::PROMPT, &to_json(&(active, at_prompt, last_exit))?),
            DaemonMsg::Exited { code } => write_frame(w, kind::EXITED, &to_json(code)?),
            DaemonMsg::PaneList(list) => write_frame(w, kind::PANE_LIST, &to_json(list)?),
            DaemonMsg::InputAck { pane_id } => write_frame(w, kind::INPUT_ACK, &to_json(pane_id)?),
            DaemonMsg::RemoteContext(remote) => {
                write_frame(w, kind::REMOTE_CONTEXT, &to_json(remote)?)
            }
            DaemonMsg::Agent(agent) => write_frame(w, kind::AGENT, &to_json(agent)?),
            DaemonMsg::AgentStatus(state) => write_frame(w, kind::AGENT_STATUS, &to_json(state)?),
            DaemonMsg::LoopbackForward(forward) => {
                write_frame(w, kind::LOOPBACK_FORWARD, &to_json(forward)?)
            }
            DaemonMsg::LoopbackForwardList(forwards) => {
                write_frame(w, kind::LOOPBACK_FORWARD_LIST, &to_json(forwards)?)
            }
            DaemonMsg::AuthPrompt { request_id, prompt } => {
                write_frame(w, kind::AUTH_PROMPT, &to_json(&(request_id, prompt))?)
            }
            DaemonMsg::SshStatus { phase } => write_frame(w, kind::SSH_STATUS, &to_json(phase)?),
            DaemonMsg::KnownHostsList(list) => {
                write_frame(w, kind::KNOWN_HOSTS_LIST, &to_json(list)?)
            }
            DaemonMsg::SftpEntries(entries) => {
                write_frame(w, kind::SFTP_ENTRIES, &to_json(entries)?)
            }
            DaemonMsg::SftpOpResult(result) => {
                write_frame(w, kind::SFTP_OP_RESULT, &to_json(result)?)
            }
            DaemonMsg::SftpTransferStarted { job_id } => {
                write_frame(w, kind::SFTP_TRANSFER_STARTED, &to_json(job_id)?)
            }
            DaemonMsg::SftpTransferProgress(jobs) => {
                write_frame(w, kind::SFTP_TRANSFER_PROGRESS, &to_json(jobs)?)
            }
            DaemonMsg::ForwardList(list) => write_frame(w, kind::FORWARD_LIST, &to_json(list)?),
            DaemonMsg::Procs(procs) => write_frame(w, kind::PROCS, &to_json(procs)?),
            DaemonMsg::Version(version) => write_frame(w, kind::VERSION_REPLY, &to_json(version)?),
            DaemonMsg::Error(msg) => write_frame(w, kind::ERROR, &to_json(msg)?),
        }
    }

    pub fn from_frame(k: u8, payload: Vec<u8>) -> io::Result<Self> {
        Ok(match k {
            kind::SPAWNED => DaemonMsg::Spawned {
                pane_id: from_json(&payload)?,
            },
            kind::SIZE => DaemonMsg::Size(from_json(&payload)?),
            kind::SNAPSHOT => DaemonMsg::Snapshot(payload),
            kind::OUTPUT => DaemonMsg::Output(payload),
            kind::IMAGE => DaemonMsg::Image(payload),
            kind::DELETE_IMAGE => DaemonMsg::DeleteImage(payload),
            kind::CWD => DaemonMsg::Cwd(from_json(&payload)?),
            kind::PROMPT => {
                let (active, at_prompt, last_exit) = from_json(&payload)?;
                DaemonMsg::Prompt {
                    active,
                    at_prompt,
                    last_exit,
                }
            }
            kind::EXITED => DaemonMsg::Exited {
                code: from_json(&payload)?,
            },
            kind::PANE_LIST => DaemonMsg::PaneList(from_json(&payload)?),
            kind::INPUT_ACK => DaemonMsg::InputAck {
                pane_id: from_json(&payload)?,
            },
            kind::REMOTE_CONTEXT => DaemonMsg::RemoteContext(from_json(&payload)?),
            kind::AGENT => DaemonMsg::Agent(from_json(&payload)?),
            kind::AGENT_STATUS => DaemonMsg::AgentStatus(from_json(&payload)?),
            kind::LOOPBACK_FORWARD => DaemonMsg::LoopbackForward(from_json(&payload)?),
            kind::LOOPBACK_FORWARD_LIST => DaemonMsg::LoopbackForwardList(from_json(&payload)?),
            kind::AUTH_PROMPT => {
                let (request_id, prompt) = from_json(&payload)?;
                DaemonMsg::AuthPrompt { request_id, prompt }
            }
            kind::SSH_STATUS => DaemonMsg::SshStatus {
                phase: from_json(&payload)?,
            },
            kind::KNOWN_HOSTS_LIST => DaemonMsg::KnownHostsList(from_json(&payload)?),
            kind::SFTP_ENTRIES => DaemonMsg::SftpEntries(from_json(&payload)?),
            kind::SFTP_OP_RESULT => DaemonMsg::SftpOpResult(from_json(&payload)?),
            kind::SFTP_TRANSFER_STARTED => DaemonMsg::SftpTransferStarted {
                job_id: from_json(&payload)?,
            },
            kind::SFTP_TRANSFER_PROGRESS => DaemonMsg::SftpTransferProgress(from_json(&payload)?),
            kind::FORWARD_LIST => DaemonMsg::ForwardList(from_json(&payload)?),
            kind::PROCS => DaemonMsg::Procs(from_json(&payload)?),
            kind::VERSION_REPLY => DaemonMsg::Version(from_json(&payload)?),
            kind::ERROR => DaemonMsg::Error(from_json(&payload)?),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown DaemonMsg kind {other}"),
                ));
            }
        })
    }

    pub fn read<R: Read>(r: &mut R) -> io::Result<Self> {
        let (k, payload) = read_frame(r)?;
        Self::from_frame(k, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: WinSize = WinSize {
        cols: 80,
        rows: 24,
        cell_w: 8,
        cell_h: 17,
    };

    #[test]
    fn full_session_round_trips_over_a_real_duplex_stream() {
        use std::io::Write;
        use std::net::{TcpListener, TcpStream};
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let client_msgs = vec![
            ClientMsg::Spawn {
                cwd: Some(PathBuf::from("/work")),
                size: SIZE,
                shell: None,
                owner: None,
                workspace: None,
                restore: None,
            },
            ClientMsg::Resize(SIZE),
            ClientMsg::Input(vec![b'l', b's', b'\r']),
            ClientMsg::Detach,
        ];
        let daemon_msgs = vec![
            DaemonMsg::Spawned { pane_id: 9 },
            DaemonMsg::Snapshot(vec![0x1b, b'[', b'2', b'J']),
            DaemonMsg::Output(b"hello\r\n".to_vec()),
            DaemonMsg::Prompt {
                active: true,
                at_prompt: true,
                last_exit: Some(0),
            },
            DaemonMsg::Exited { code: Some(0) },
        ];

        let expect_from_client = client_msgs.clone();
        let reply_with = daemon_msgs.clone();
        let daemon = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let got: Vec<ClientMsg> = (0..expect_from_client.len())
                .map(|_| ClientMsg::read(&mut sock).unwrap())
                .collect();
            for m in &reply_with {
                m.encode(&mut sock).unwrap();
            }
            sock.flush().unwrap();
            got
        });

        let mut sock = TcpStream::connect(addr).unwrap();
        for m in &client_msgs {
            m.encode(&mut sock).unwrap();
        }
        sock.flush().unwrap();
        let got_from_daemon: Vec<DaemonMsg> = (0..daemon_msgs.len())
            .map(|_| DaemonMsg::read(&mut sock).unwrap())
            .collect();

        let got_from_client = daemon.join().unwrap();
        assert_eq!(got_from_client, client_msgs, "daemon decoded client stream");
        assert_eq!(got_from_daemon, daemon_msgs, "client decoded daemon stream");
    }

    #[test]
    fn client_roundtrip() {
        let msgs = vec![
            ClientMsg::Spawn {
                cwd: Some(PathBuf::from("/tmp/x")),
                size: SIZE,
                shell: None,
                owner: None,
                workspace: None,
                restore: None,
            },
            ClientMsg::Spawn {
                cwd: None,
                size: SIZE,
                shell: None,
                owner: None,
                workspace: None,
                restore: None,
            },
            ClientMsg::Spawn {
                cwd: Some(PathBuf::from("/tmp/x")),
                size: SIZE,
                shell: Some(ShellSpec {
                    program: "wsl.exe".into(),
                    args: vec!["--distribution".into(), "Ubuntu".into()],
                    args_are_tty7_defaults: true,
                }),
                owner: None,
                workspace: None,
                restore: None,
            },
            ClientMsg::Spawn {
                cwd: Some(PathBuf::from("/tmp/x")),
                size: SIZE,
                shell: None,
                owner: Some("bda10e44-02de-44a0-8412-ec1cda2b5f5b".into()),
                workspace: None,
                restore: None,
            },
            ClientMsg::Spawn {
                cwd: Some(PathBuf::from("/tmp/x")),
                size: SIZE,
                shell: None,
                owner: None,
                workspace: Some("ws-main".into()),
                restore: None,
            },
            ClientMsg::Observe {
                pane_id: 42,
                size: SIZE,
            },
            ClientMsg::Attach {
                pane_id: 42,
                size: SIZE,
            },
            ClientMsg::Input(vec![0x1b, b'[', b'A', 0, 255]),
            ClientMsg::SendInput {
                pane_id: 42,
                bytes: vec![b'l', b's', b'\r', 0, 255],
            },
            ClientMsg::SendInput {
                pane_id: 7,
                bytes: Vec::new(),
            },
            ClientMsg::Resize(SIZE),
            ClientMsg::Detach,
            ClientMsg::Kill { pane_id: 7 },
            ClientMsg::List,
            ClientMsg::Shutdown,
            ClientMsg::EnsureLoopbackForward(LoopbackForwardRequest {
                pane_id: 7,
                remote_host: "127.0.0.1".into(),
                remote_port: 3000,
            }),
            ClientMsg::ListLoopbackForwards,
            ClientMsg::CloseLoopbackForward(LoopbackForwardId {
                pane_id: 7,
                target: "dev".into(),
                remote_host: "127.0.0.1".into(),
                remote_port: 3000,
            }),
            ClientMsg::ListKnownHosts,
            ClientMsg::DeleteKnownHost(KnownHostId {
                host: "example.com".into(),
                key_type: "ssh-ed25519".into(),
                keyblob: "AAAAC3Nz".into(),
            }),
            ClientMsg::SftpList {
                pane_id: 4,
                path: "/home/deploy/项目".into(),
            },
            ClientMsg::SftpOp {
                pane_id: 4,
                op: SftpOp::Mkdir {
                    path: "/tmp/new dir".into(),
                },
            },
            ClientMsg::SftpOp {
                pane_id: 4,
                op: SftpOp::Rename {
                    from: "/a".into(),
                    to: "/b".into(),
                },
            },
            ClientMsg::SftpOp {
                pane_id: 4,
                op: SftpOp::Chmod {
                    path: "/x".into(),
                    mode: 0o755,
                },
            },
            ClientMsg::SftpOp {
                pane_id: 4,
                op: SftpOp::Readlink {
                    path: "/link".into(),
                },
            },
            ClientMsg::SftpOp {
                pane_id: 4,
                op: SftpOp::Realpath { path: ".".into() },
            },
            ClientMsg::SftpTransferStart(SftpTransferSpec {
                pane_id: 4,
                kind: SftpTransferKind::Upload,
                local: PathBuf::from("/local/f"),
                remote: "/remote/f".into(),
                recursive: true,
            }),
            ClientMsg::SftpTransferCancel { job_id: 9 },
            ClientMsg::SftpTransferList { pane_id: 4 },
            ClientMsg::AddForward {
                pane_id: 7,
                rule: SshForwardRule {
                    kind: SshForwardKind::Local,
                    bind_host: "127.0.0.1".into(),
                    bind_port: 8080,
                    target_host: "10.0.0.5".into(),
                    target_port: 80,
                    description: Some("web".into()),
                },
            },
            ClientMsg::AddForward {
                pane_id: 7,
                rule: SshForwardRule {
                    kind: SshForwardKind::Dynamic,
                    bind_host: "127.0.0.1".into(),
                    bind_port: 1080,
                    target_host: String::new(),
                    target_port: 0,
                    description: None,
                },
            },
            ClientMsg::RemoveForward {
                pane_id: 7,
                forward_id: 3,
            },
            ClientMsg::ListForwards { pane_id: 7 },
            ClientMsg::Version,
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            m.encode(&mut buf).unwrap();
        }
        let mut cursor = std::io::Cursor::new(buf);
        for m in &msgs {
            assert_eq!(*m, ClientMsg::read(&mut cursor).unwrap());
        }
    }

    #[test]
    fn daemon_roundtrip() {
        let msgs = vec![
            DaemonMsg::Spawned { pane_id: 1 },
            DaemonMsg::Size(SIZE),
            DaemonMsg::Snapshot(vec![1, 2, 3, 0, 255]),
            DaemonMsg::Output((0u8..=255).collect()),
            DaemonMsg::Cwd(PathBuf::from("/home/u/dev")),
            DaemonMsg::Prompt {
                active: true,
                at_prompt: false,
                last_exit: Some(130),
            },
            DaemonMsg::Exited { code: Some(0) },
            DaemonMsg::Exited { code: None },
            DaemonMsg::PaneList(vec![
                PaneInfo {
                    pane_id: 3,
                    cwd: Some(PathBuf::from("/x")),
                    title: "zsh".into(),
                    alive: true,
                    owner: None,
                },
                PaneInfo {
                    pane_id: 4,
                    cwd: None,
                    title: String::new(),
                    alive: true,
                    owner: Some("ffe038d0-9ad6-40c0-815d-1fcc43c17ec0".into()),
                },
            ]),
            DaemonMsg::InputAck { pane_id: 42 },
            DaemonMsg::RemoteContext(Some(RemoteContext {
                kind: RemoteKind::Ssh,
                argv: vec!["ssh".into(), "-p".into(), "2222".into(), "dev".into()],
                target: "dev".into(),
            })),
            DaemonMsg::RemoteContext(Some(RemoteContext {
                kind: RemoteKind::Wsl,
                argv: Vec::new(),
                target: "Ubuntu-24.04".into(),
            })),
            DaemonMsg::RemoteContext(None),
            DaemonMsg::Agent(Some(crate::core::cli_agent::CLIAgent::Claude)),
            DaemonMsg::Agent(Some(crate::core::cli_agent::CLIAgent::Codex)),
            DaemonMsg::Agent(None),
            DaemonMsg::AgentStatus(Some(crate::core::cli_agent::AgentSessionState {
                status: crate::core::cli_agent::AgentStatus::Waiting,
                message: Some("Claude needs your permission to use Bash".into()),
                session_id: Some("abc-123".into()),
                launch_argv: Some(vec![
                    "claude".into(),
                    "--dangerously-skip-permissions".into(),
                ]),
                rich: true,
                cwd: Some("/repo/.claude/worktrees/fix-x".into()),
                activity: 12,
            })),
            DaemonMsg::AgentStatus(None),
            DaemonMsg::LoopbackForward(LoopbackForward { local_port: 49152 }),
            DaemonMsg::LoopbackForwardList(vec![LoopbackForwardInfo {
                id: LoopbackForwardId {
                    pane_id: 7,
                    target: "dev".into(),
                    remote_host: "127.0.0.1".into(),
                    remote_port: 3000,
                },
                local_port: 49152,
                age_secs: 12,
                idle_secs: 3,
            }]),
            DaemonMsg::KnownHostsList(vec![KnownHostEntry {
                host: "example.com".into(),
                marker: Some("@revoked".into()),
                key_type: "ssh-ed25519".into(),
                fingerprint_sha256: "SHA256:abc".into(),
                id: KnownHostId {
                    host: "example.com".into(),
                    key_type: "ssh-ed25519".into(),
                    keyblob: "AAAAC3Nz".into(),
                },
            }]),
            DaemonMsg::SftpEntries(vec![
                SftpEntry {
                    name: "src".into(),
                    kind: SftpEntryKind::Dir,
                    size: 4096,
                    mtime: 1_700_000_000,
                    permissions: 0o40755,
                    target_is_dir: false,
                },
                SftpEntry {
                    name: "链接".into(),
                    kind: SftpEntryKind::Symlink,
                    size: 0,
                    mtime: 0,
                    permissions: 0o120777,
                    target_is_dir: true,
                },
            ]),
            DaemonMsg::SftpOpResult(SftpOpResult::Done),
            DaemonMsg::SftpOpResult(SftpOpResult::Link("/target/path".into())),
            DaemonMsg::SftpOpResult(SftpOpResult::Error("permission denied".into())),
            DaemonMsg::SftpOpResult(SftpOpResult::Stat(SftpEntry {
                name: "file".into(),
                kind: SftpEntryKind::File,
                size: 12,
                mtime: 5,
                permissions: 0o100644,
                target_is_dir: false,
            })),
            DaemonMsg::SftpTransferStarted { job_id: 3 },
            DaemonMsg::SftpTransferProgress(vec![SftpJobProgress {
                job_id: 3,
                pane_id: 4,
                kind: SftpTransferKind::Download,
                state: SftpJobState::Running,
                current: "big.iso".into(),
                bytes_done: 1024,
                bytes_total: 4096,
                error: None,
                local: "/local".into(),
                remote: "/remote".into(),
            }]),
            DaemonMsg::ForwardList(vec![
                ManagedForward {
                    id: 1,
                    pane_id: 7,
                    kind: SshForwardKind::Local,
                    bind_host: "127.0.0.1".into(),
                    bind_port: 8080,
                    target_host: "10.0.0.5".into(),
                    target_port: 80,
                    description: Some("web".into()),
                    status: ForwardStatus::Listening,
                },
                ManagedForward {
                    id: 2,
                    pane_id: 7,
                    kind: SshForwardKind::Remote,
                    bind_host: "0.0.0.0".into(),
                    bind_port: 9000,
                    target_host: "127.0.0.1".into(),
                    target_port: 3000,
                    description: None,
                    status: ForwardStatus::Error("bind refused".into()),
                },
            ]),
            DaemonMsg::Version(DaemonVersion {
                protocol: PROTOCOL_VERSION,
                build: "0.15.0".into(),
                features: vec!["control".into(), "host-rpc".into()],
                instance: "inst-a".into(),
            }),
            DaemonMsg::Version(DaemonVersion {
                protocol: PROTOCOL_VERSION,
                build: "0.15.0".into(),
                features: Vec::new(),
                instance: String::new(),
            }),
            DaemonMsg::Error("nope".into()),
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            m.encode(&mut buf).unwrap();
        }
        let mut cursor = std::io::Cursor::new(buf);
        for m in &msgs {
            assert_eq!(*m, DaemonMsg::read(&mut cursor).unwrap());
        }
    }

    #[test]
    fn default_spawn_stays_wire_compatible_with_old_daemons() {
        let msg = ClientMsg::Spawn {
            cwd: Some(PathBuf::from("/work")),
            size: SIZE,
            shell: None,
            owner: None,
            workspace: None,
            restore: None,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let (k, payload) = read_frame(&mut std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(k, kind::SPAWN, "default spawn must use the legacy kind");
        let (cwd, size): (Option<PathBuf>, WinSize) = serde_json::from_slice(&payload).unwrap();
        assert_eq!(cwd, Some(PathBuf::from("/work")));
        assert_eq!(size, SIZE);

        let legacy = serde_json::to_vec(&(Some(PathBuf::from("/old")), SIZE)).unwrap();
        let decoded = ClientMsg::from_frame(kind::SPAWN, legacy).unwrap();
        assert_eq!(
            decoded,
            ClientMsg::Spawn {
                cwd: Some(PathBuf::from("/old")),
                size: SIZE,
                shell: None,
                owner: None,
                workspace: None,
                restore: None,
            }
        );
    }

    #[test]
    fn explicit_shell_spawn_uses_shell_kind() {
        let shell = ShellSpec {
            program: "fish".to_string(),
            args: vec!["-l".to_string()],
            args_are_tty7_defaults: true,
        };
        let msg = ClientMsg::Spawn {
            cwd: Some(PathBuf::from("/work")),
            size: SIZE,
            shell: Some(shell.clone()),
            owner: None,
            workspace: None,
            restore: None,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let (k, payload) = read_frame(&mut std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(k, kind::SPAWN_SHELL);
        let decoded = ClientMsg::from_frame(k, payload).unwrap();
        assert_eq!(
            decoded,
            ClientMsg::Spawn {
                cwd: Some(PathBuf::from("/work")),
                size: SIZE,
                shell: Some(shell),
                owner: None,
                workspace: None,
                restore: None,
            }
        );
    }

    #[test]
    fn owned_spawn_uses_the_owned_kind_and_round_trips() {
        let msg = ClientMsg::Spawn {
            cwd: Some(PathBuf::from("/work")),
            size: SIZE,
            shell: Some(ShellSpec {
                program: "fish".into(),
                args: vec!["-l".into()],
                args_are_tty7_defaults: false,
            }),
            owner: Some("bda10e44-02de-44a0-8412-ec1cda2b5f5b".into()),
            workspace: Some("ws-7".into()),
            restore: None,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let (k, payload) = read_frame(&mut std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(k, kind::SPAWN_OWNED);
        assert_eq!(ClientMsg::from_frame(k, payload).unwrap(), msg);
    }

    #[test]
    fn owned_spawn_payload_tolerates_unknown_and_missing_fields() {
        let payload = serde_json::to_vec(&serde_json::json!({
            "size": {"cols": 80, "rows": 24, "cell_w": 8, "cell_h": 17},
            "some_future_field": true,
        }))
        .unwrap();
        let decoded = ClientMsg::from_frame(kind::SPAWN_OWNED, payload).unwrap();
        assert_eq!(
            decoded,
            ClientMsg::Spawn {
                cwd: None,
                size: WinSize {
                    cols: 80,
                    rows: 24,
                    cell_w: 8,
                    cell_h: 17
                },
                shell: None,
                owner: None,
                workspace: None,
                restore: None,
            }
        );
    }

    #[test]
    fn a_workspace_spawn_uses_the_owned_kind_and_round_trips() {
        let msg = ClientMsg::Spawn {
            cwd: None,
            size: SIZE,
            shell: None,
            owner: None,
            workspace: Some("ws-main".into()),
            restore: None,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let (k, payload) = read_frame(&mut std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(
            k,
            kind::SPAWN_OWNED,
            "a workspace-tagged spawn must not ride the legacy kinds, which drop the field"
        );
        assert_eq!(ClientMsg::from_frame(k, payload).unwrap(), msg);
    }

    #[test]
    fn observe_uses_its_own_kind_and_round_trips() {
        let msg = ClientMsg::Observe {
            pane_id: 42,
            size: SIZE,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let (k, payload) = read_frame(&mut std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(k, kind::OBSERVE);
        assert_ne!(k, kind::ATTACH, "observing must never preempt an attach");
        assert_eq!(ClientMsg::from_frame(k, payload).unwrap(), msg);
    }

    #[test]
    fn pane_info_owner_defaults_for_old_daemons() {
        let old = serde_json::json!({"pane_id": 3, "title": "zsh", "alive": true});
        let info: PaneInfo = serde_json::from_value(old).unwrap();
        assert_eq!(info.owner, None);
        assert!(info.alive);
    }

    #[test]
    fn frame_edges() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 3, &[]).unwrap();
        let mut cursor = std::io::Cursor::new(&buf);
        assert_eq!(read_frame(&mut cursor).unwrap(), (3, vec![]));

        let mut bad = Vec::new();
        bad.extend_from_slice(&(u32::MAX).to_le_bytes());
        bad.push(3);
        let mut cursor = std::io::Cursor::new(&bad);
        assert!(read_frame(&mut cursor).is_err());
    }

    #[test]
    fn write_frame_rejects_oversize_payload() {
        let oversize = vec![0u8; MAX_FRAME + 1];
        let mut buf = Vec::new();
        assert!(write_frame(&mut buf, 3, &oversize).is_err());
        assert!(buf.is_empty());
    }

    #[test]
    fn from_frame_rejects_unknown_kind() {
        assert!(ClientMsg::from_frame(99, vec![]).is_err());
        assert!(DaemonMsg::from_frame(99, vec![]).is_err());
    }

    #[test]
    fn take_frame_is_resumable_and_mirrors_read_frame() {
        let mut wire = Vec::new();
        write_frame(&mut wire, 3, b"hello").unwrap();
        write_frame(&mut wire, 9, &[]).unwrap();

        let mut buf = Vec::new();
        let mut got = Vec::new();
        for &b in &wire {
            buf.push(b);
            while let Some(frame) = take_frame(&mut buf).unwrap() {
                got.push(frame);
            }
        }
        assert_eq!(got, vec![(3, b"hello".to_vec()), (9, vec![])]);
        assert!(buf.is_empty(), "nothing left over after both frames");

        let mut buf = Vec::new();
        write_frame(&mut buf, 3, b"done").unwrap();
        buf.extend_from_slice(&10u32.to_le_bytes());
        assert_eq!(take_frame(&mut buf).unwrap(), Some((3, b"done".to_vec())));
        assert_eq!(take_frame(&mut buf).unwrap(), None);
        assert_eq!(buf, 10u32.to_le_bytes());

        let mut bad = (u32::MAX).to_le_bytes().to_vec();
        bad.push(3);
        assert!(take_frame(&mut bad).is_err());
    }

    #[test]
    fn read_frame_on_truncated_frame_is_an_error() {
        let mut cut = std::io::Cursor::new(5u32.to_le_bytes().to_vec());
        assert_eq!(
            read_frame(&mut cut).unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );

        let mut buf = Vec::new();
        buf.extend_from_slice(&10u32.to_le_bytes());
        buf.push(3);
        buf.extend_from_slice(b"only4");
        let mut cut = std::io::Cursor::new(buf);
        assert_eq!(
            read_frame(&mut cut).unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn from_frame_rejects_malformed_json_payloads() {
        assert!(ClientMsg::from_frame(kind::SPAWN, b"not json".to_vec()).is_err());
        assert!(DaemonMsg::from_frame(kind::PANE_LIST, b"{oops".to_vec()).is_err());
    }

    #[test]
    fn read_frame_on_empty_input_is_eof() {
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        let err = read_frame(&mut empty).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        let mut empty2 = std::io::Cursor::new(Vec::<u8>::new());
        assert!(ClientMsg::read(&mut empty2).is_err());
    }

    #[test]
    fn pane_info_deserializes_with_defaults() {
        let info: PaneInfo = serde_json::from_str(r#"{"pane_id": 5, "alive": true}"#).unwrap();
        assert_eq!(info.pane_id, 5);
        assert!(info.alive);
        assert_eq!(info.cwd, None);
        assert_eq!(info.title, "");
    }

    /// The reason `previously_known_as` is a field and not a variant: this
    /// prompt is decoded by whatever GUI or `tty7-server` is at the other end,
    /// and one of them is regularly older than the build that sent it. A
    /// missing field defaults; an unknown variant would fail the whole frame.
    #[test]
    fn an_unknown_host_prompt_from_an_older_peer_still_decodes() {
        let prompt: AuthPromptKind = serde_json::from_str(
            r#"{"HostKeyUnknown":{"host":"h","port":22,"algorithm":"ssh-ed25519","fingerprint_sha256":"SHA256:x"}}"#,
        )
        .unwrap();
        assert_eq!(
            prompt,
            AuthPromptKind::HostKeyUnknown {
                host: "h".into(),
                port: 22,
                algorithm: "ssh-ed25519".into(),
                fingerprint_sha256: "SHA256:x".into(),
                previously_known_as: None,
            }
        );
    }

    fn sample_native_spec() -> NativeSshSpec {
        let mut passphrases = std::collections::HashMap::new();
        passphrases.insert("~/.ssh/id_ed25519".to_string(), "topsecret".to_string());
        NativeSshSpec {
            host: "example.com".into(),
            port: 2222,
            user: "deploy".into(),
            auth_mode: SshAuthMode::Auto,
            identity_files: vec!["~/.ssh/id_ed25519".into()],
            agent_forward: true,
            password: Some("hunter2".into()),
            key_passphrases: Some(passphrases),
            proxy: SshProxy::Socks {
                host: "127.0.0.1".into(),
                port: 1080,
            },
            jump: Some(Box::new(NativeSshSpec {
                host: "bastion".into(),
                port: 22,
                user: "jump".into(),
                auth_mode: SshAuthMode::Agent,
                identity_files: vec![],
                agent_forward: false,
                password: Some("jumppass".into()),
                key_passphrases: None,
                proxy: SshProxy::None,
                jump: None,
                forwards: vec![],
                keepalive_interval_s: None,
                keepalive_count_max: None,
                connect_timeout_s: None,
                algorithms: SshAlgorithms::default(),
                x11: false,
                term: "xterm-256color".into(),
                verify_host_keys: true,
                skip_banner: false,
                shell_integration: true,
                login_script: vec![],
                display_name: None,
                profile_id: None,
            })),
            forwards: vec![SshForwardRule {
                kind: SshForwardKind::Local,
                bind_host: "127.0.0.1".into(),
                bind_port: 8000,
                target_host: "127.0.0.1".into(),
                target_port: 80,
                description: Some("web".into()),
            }],
            keepalive_interval_s: Some(30),
            keepalive_count_max: Some(3),
            connect_timeout_s: Some(20),
            algorithms: SshAlgorithms {
                cipher: vec!["aes256-ctr".into()],
                ..Default::default()
            },
            x11: true,
            term: "xterm-256color".into(),
            verify_host_keys: true,
            skip_banner: false,
            shell_integration: true,
            login_script: vec!["tmux attach".into()],
            display_name: Some("prod-web".into()),
            profile_id: Some("uuid-1".into()),
        }
    }

    #[test]
    fn native_ssh_spec_serde_round_trips() {
        let spec = sample_native_spec();
        let json = serde_json::to_string(&spec).unwrap();
        let back: NativeSshSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
    }

    #[test]
    fn native_ssh_spec_tolerates_minimal_json() {
        let spec: NativeSshSpec =
            serde_json::from_str(r#"{"host":"h","port":22,"user":"u","auth_mode":"auto"}"#)
                .unwrap();
        assert_eq!(spec.term, "xterm-256color");
        assert!(spec.verify_host_keys);
        assert!(spec.shell_integration);
        assert_eq!(spec.password, None);
        assert!(spec.jump.is_none());
    }

    #[test]
    fn secrets_are_redacted_in_debug_output() {
        let spec = sample_native_spec();
        let dbg = format!("{spec:?}");
        assert!(!dbg.contains("hunter2"), "password leaked: {dbg}");
        assert!(!dbg.contains("topsecret"), "passphrase leaked: {dbg}");
        assert!(!dbg.contains("jumppass"), "jump password leaked: {dbg}");
        assert!(dbg.contains("<redacted>"));

        assert_eq!(
            format!("{:?}", AuthResponse::Secret("pw".into())),
            "Secret(<redacted>)"
        );
        assert_eq!(
            format!("{:?}", AuthResponse::Secrets(vec!["a".into(), "b".into()])),
            "Secrets(<2 redacted>)"
        );
    }

    #[test]
    fn without_secrets_strips_password_and_passphrases_recursively() {
        let clean = sample_native_spec().without_secrets();
        assert_eq!(clean.password, None);
        assert!(clean.key_passphrases.is_none());
        assert_eq!(clean.jump.as_ref().unwrap().password, None);
        assert_eq!(clean.host, "example.com");
        assert_eq!(clean.login_script, vec!["tmux attach".to_string()]);
    }

    #[test]
    fn on_workspace_roundtrip() {
        let ws = crate::core::session::WorkspaceId::new();
        let spec = Box::new(sample_native_spec().without_secrets());
        let ops = vec![
            WorkspaceOp::EnsureLoopback {
                remote_host: "127.0.0.1".into(),
                remote_port: 3000,
            },
            WorkspaceOp::AddForward {
                rule: SshForwardRule {
                    kind: SshForwardKind::Local,
                    bind_host: "127.0.0.1".into(),
                    bind_port: 0,
                    target_host: "127.0.0.1".into(),
                    target_port: 5432,
                    description: Some("db".into()),
                },
            },
            WorkspaceOp::RemoveForward { forward_id: 4 },
            WorkspaceOp::ListForwards,
            WorkspaceOp::TeardownForwards,
            WorkspaceOp::SftpList {
                path: "/home/me".into(),
            },
            WorkspaceOp::SftpOp {
                op: SftpOp::Realpath { path: ".".into() },
            },
            WorkspaceOp::SftpTransferStart {
                spec: SftpTransferSpec {
                    pane_id: 0,
                    kind: SftpTransferKind::Download,
                    local: PathBuf::from("/local/f"),
                    remote: "/remote/f".into(),
                    recursive: false,
                },
            },
            WorkspaceOp::SftpTransferList,
        ];
        let msgs: Vec<ClientMsg> = ops
            .into_iter()
            .map(|op| {
                ClientMsg::OnWorkspace(Box::new(WorkspaceRequest {
                    workspace: ws,
                    spec: spec.clone(),
                    view_pane: 12,
                    op,
                }))
            })
            .collect();
        let mut buf = Vec::new();
        for m in &msgs {
            m.encode(&mut buf).unwrap();
        }
        let mut cursor = std::io::Cursor::new(buf);
        for m in &msgs {
            assert_eq!(*m, ClientMsg::read(&mut cursor).unwrap());
        }
    }

    #[test]
    fn native_ssh_messages_round_trip() {
        let client_msgs = vec![
            ClientMsg::SpawnNativeSsh {
                cwd: Some(PathBuf::from("/work")),
                size: SIZE,
                spec: Box::new(sample_native_spec()),
            },
            ClientMsg::AuthResponse {
                request_id: 7,
                response: AuthResponse::Secret("pw".into()),
            },
            ClientMsg::AuthResponse {
                request_id: 8,
                response: AuthResponse::HostKeyDecision {
                    accept: true,
                    remember: true,
                },
            },
        ];
        let mut buf = Vec::new();
        for m in &client_msgs {
            m.encode(&mut buf).unwrap();
        }
        let mut cur = std::io::Cursor::new(buf);
        for m in &client_msgs {
            assert_eq!(*m, ClientMsg::read(&mut cur).unwrap());
        }

        let daemon_msgs = vec![
            DaemonMsg::AuthPrompt {
                request_id: 1,
                prompt: AuthPromptKind::HostKeyChanged {
                    host: "h".into(),
                    port: 22,
                    algorithm: "ssh-ed25519".into(),
                    fingerprint_sha256: "SHA256:new".into(),
                    old_fingerprint_sha256: "SHA256:old".into(),
                },
            },
            DaemonMsg::AuthPrompt {
                request_id: 2,
                prompt: AuthPromptKind::KeyboardInteractive {
                    name: "2FA".into(),
                    instructions: "enter code".into(),
                    prompts: vec![KiPrompt {
                        text: "Code:".into(),
                        echo: true,
                    }],
                    stored_rejected: true,
                },
            },
            DaemonMsg::AuthPrompt {
                request_id: 3,
                prompt: AuthPromptKind::KeyPassphrase {
                    key_path: "~/.ssh/id_ed25519".into(),
                    comment: String::new(),
                    rejected: true,
                },
            },
            DaemonMsg::SshStatus {
                phase: SshPhase::Failed {
                    reason: "nope".into(),
                },
            },
        ];
        let mut buf = Vec::new();
        for m in &daemon_msgs {
            m.encode(&mut buf).unwrap();
        }
        let mut cur = std::io::Cursor::new(buf);
        for m in &daemon_msgs {
            assert_eq!(*m, DaemonMsg::read(&mut cur).unwrap());
        }
    }

    /// `rejected` rides on a struct variant of an externally tagged enum that
    /// crosses both daemon↔GUI and GUI↔remote `tty7-server`, so it has to
    /// survive a peer that predates it in *either* direction. That is why
    /// `PROTOCOL_VERSION` did not move for it: the remote handshake gates on
    /// it, and bumping would turn every older server away over a field it can
    /// safely ignore.
    #[test]
    fn a_rejected_passphrase_flag_decodes_from_a_peer_that_never_sends_it() {
        let old = r#"{"KeyPassphrase":{"key_path":"~/.ssh/id_ed25519","comment":""}}"#;
        assert_eq!(
            serde_json::from_str::<AuthPromptKind>(old).unwrap(),
            AuthPromptKind::KeyPassphrase {
                key_path: "~/.ssh/id_ed25519".into(),
                comment: String::new(),
                rejected: false,
            }
        );

        // The other direction: a peer that predates the flag is handed one
        // set, and must read the prompt rather than reject the frame.
        let new = serde_json::to_string(&AuthPromptKind::KeyPassphrase {
            key_path: "/k".into(),
            comment: "work laptop".into(),
            rejected: true,
        })
        .unwrap();
        #[derive(Deserialize)]
        enum LegacyPromptKind {
            KeyPassphrase { key_path: String, comment: String },
        }
        let LegacyPromptKind::KeyPassphrase { key_path, comment } =
            serde_json::from_str::<LegacyPromptKind>(&new).unwrap();
        assert_eq!(key_path, "/k");
        assert_eq!(comment, "work laptop");
    }

    /// `stored_rejected` gets the same treatment as `rejected` above, for the
    /// same reason and with the same `PROTOCOL_VERSION` left alone.
    #[test]
    fn a_stored_rejected_flag_decodes_from_a_peer_that_never_sends_it() {
        let old = r#"{"KeyboardInteractive":{"name":"2FA","instructions":"","prompts":[]}}"#;
        assert_eq!(
            serde_json::from_str::<AuthPromptKind>(old).unwrap(),
            AuthPromptKind::KeyboardInteractive {
                name: "2FA".into(),
                instructions: String::new(),
                prompts: vec![],
                stored_rejected: false,
            }
        );

        let new = serde_json::to_string(&AuthPromptKind::KeyboardInteractive {
            name: "2FA".into(),
            instructions: "code".into(),
            prompts: vec![],
            stored_rejected: true,
        })
        .unwrap();
        #[derive(Deserialize)]
        enum LegacyPromptKind {
            KeyboardInteractive { name: String, instructions: String },
        }
        let LegacyPromptKind::KeyboardInteractive { name, instructions } =
            serde_json::from_str::<LegacyPromptKind>(&new).unwrap();
        assert_eq!(name, "2FA");
        assert_eq!(instructions, "code");
    }

    #[test]
    fn native_ssh_spawn_uses_new_kind_byte() {
        let msg = ClientMsg::SpawnNativeSsh {
            cwd: None,
            size: SIZE,
            spec: Box::new(sample_native_spec()),
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let (k, _) = read_frame(&mut std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(k, kind::SPAWN_NATIVE_SSH);
    }

    #[test]
    fn a_version_reply_without_features_still_decodes() {
        let legacy = br#"{"protocol":2,"build":"26.7.4"}"#;
        let v: DaemonVersion = serde_json::from_slice(legacy).unwrap();
        assert_eq!(v.protocol, 2);
        assert_eq!(v.build, "26.7.4");
        assert!(v.features.is_empty());
        assert!(!v.has_feature(crate::daemon::control::feature::CONTROL));
    }

    #[test]
    fn a_version_reply_with_unknown_fields_still_decodes() {
        let future = br#"{"protocol":4,"build":"99.0.0","features":["control"],
                          "something_new":{"a":1}}"#;
        let v: DaemonVersion = serde_json::from_slice(future).unwrap();
        assert_eq!(v.protocol, 4);
        assert!(v.has_feature(crate::daemon::control::feature::CONTROL));
    }

    #[test]
    fn the_local_daemon_does_not_claim_the_control_dialect() {
        let v = DaemonVersion::current();
        assert_eq!(v.protocol, 5);
        assert!(
            !v.has_feature(crate::daemon::control::feature::CONTROL),
            "the session daemon must not advertise a dialect it cannot serve"
        );
    }
}

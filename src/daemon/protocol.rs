//! Wire protocol between the GUI **client** and the persistent **daemon**.
//!
//! One Unix-domain-socket connection carries exactly one *pane* (a single PTY +
//! child). The GUI opens one connection per terminal view; session listing uses
//! a short-lived control connection. This mirrors the in-process model where one
//! `TerminalView` owns one terminal, so nothing higher up needs multiplexing.
//!
//! ## Framing
//!
//! Every message is a length-prefixed frame:
//!
//! ```text
//! [u32 LE payload_len][u8 kind][payload (payload_len bytes)]
//! ```
//!
//! The `kind` byte selects the variant. Hot-path variants (`Input`, `Output`,
//! `Snapshot`) carry the raw PTY bytes *verbatim* as the payload — no
//! serialization, no copy beyond the frame. Cold control variants serialize
//! their small structs as JSON, which keeps the wire format easy to evolve and
//! debug without pulling in a binary-codec dependency.
//!
//! Decoding never trusts the length blindly: frames larger than [`MAX_FRAME`]
//! are rejected so a desynced/hostile peer can't make us allocate unboundedly.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Upper bound on a single frame's payload. A `Snapshot` replays the daemon's
/// byte ring (a few MB by default), so this is generous; anything past it is a
/// protocol desync and we error rather than allocate.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// Terminal geometry shared by spawn/attach/resize. Cell pixel size travels too
/// so the daemon can set an accurate `TIOCSWINSZ` (`ws_xpixel`/`ws_ypixel`),
/// which some full-screen apps read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u16,
    pub cell_h: u16,
}

/// A shell program plus launch arguments, carried by `Spawn` when the user
/// picked a specific shell from the new-tab dropdown. Same shape as
/// `config::ShellConfig`, but defined here so the wire format doesn't depend
/// on the config module's evolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSpec {
    /// Bare name resolved via `PATH` (`"pwsh"`) or an absolute path.
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// Whether a short `ssh` option flag consumes the following argument as its
/// value. Used by the GUI's typed-connect parser to skip an option's value while
/// hunting for the destination token.
pub(crate) fn ssh_option_takes_value(flag: char) -> bool {
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

/// Metadata for one live pane, returned by `List` for session restore / pickers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: u64,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub title: String,
    /// False once the child has exited but the pane lingers (so a client can
    /// still read its final scrollback).
    pub alive: bool,
}

/// A foreground remote session the daemon can prove from the local process table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteContext {
    pub kind: RemoteKind,
    /// Original foreground argv. Kept so follow-up operations can preserve ssh
    /// config flags such as `-F`, `-p`, and `-J` rather than guessing.
    pub argv: Vec<String>,
    /// The destination token (`host`, `user@host`, or ssh config alias).
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteKind {
    /// A foreground `ssh` process typed into a normal shell, detected from the
    /// local process table. Status/label only — it has no tty7-owned connection,
    /// so forwarding / SFTP don't apply to it.
    Ssh,
    /// A pane backed by the daemon's native russh session engine
    /// (`daemon::ssh`). Forwarding / SFTP reach the connection through the
    /// in-memory registry.
    NativeSsh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoopbackForwardRequest {
    pub pane_id: u64,
    pub remote_host: String,
    pub remote_port: u16,
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

// ---------------------------------------------------------------------------
// Native SSH (russh) session engine — wire types (Workstream 2).
//
// A `NativeSshSpec` is everything the daemon needs to establish one russh
// connection and open a shell channel on it. The GUI (WS1/WS6) resolves a
// stored profile — including any OS-keychain secrets and any jump-host profile
// references — into this fully self-contained spec before sending it; the daemon
// never reads the keychain or the profile store. Secrets (`password`,
// `key_passphrases`) ride the *local* daemon socket exactly once and are held
// only in memory. `NativeSshSpec` has a hand-written `Debug` that redacts them,
// so it is safe to log a spec for diagnostics.
// ---------------------------------------------------------------------------

/// Which authentication methods the daemon may attempt. `Auto` tries all in the
/// Tabby-derived order (none → publickey → agent → password → keyboard-interactive);
/// the others restrict attempts to that single family (plus the mandatory leading
/// `none` probe, which only learns the server's advertised methods).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SshAuthMode {
    #[default]
    Auto,
    Password,
    PublicKey,
    Agent,
    KeyboardInteractive,
}

/// The transport under the SSH connection. Exactly one is used; `Command` and the
/// proxies are mutually exclusive with each other and with a jump host (which is
/// carried separately on `NativeSshSpec::jump`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SshProxy {
    #[default]
    None,
    /// A `ProxyCommand`-style program whose stdio is the transport. The daemon
    /// substitutes `%h`/`%p` (and `%r`) tokens itself before spawning — the gap
    /// Tabby left open (#11058).
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

/// Per-connection algorithm preference lists. Empty list = russh defaults (with
/// tty7's Tabby-derived preference applied where russh supports the entry).
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

/// A preconfigured port-forward carried on the spec. WS2 only carries the data;
/// establishing forwards is WS4's job (see the seam in `daemon::ssh`).
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

// ---------------------------------------------------------------------------
// SFTP (Workstream 5) — wire types.
//
// SFTP rides a native-SSH pane's already-authenticated russh connection: the
// daemon opens an SFTP-subsystem channel on the pane's connection (reused across
// panes sharing it) and answers directory listings / file operations / transfer
// jobs. All requests carry the `pane_id`; the daemon resolves it to the pane's
// `SshConnection` through the registry. Only native-SSH panes have one — a PTY
// pane (or a foreground `ssh` typed in a shell) replies with an `Error`.
// ---------------------------------------------------------------------------

/// The classification of one remote directory entry. Symlinks are reported as
/// `Symlink`; the daemon additionally follow-stats the target so the GUI can tell
/// a link-to-directory (navigable) from a link-to-file (downloadable) via
/// [`SftpEntry::target_is_dir`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SftpEntryKind {
    File,
    Dir,
    Symlink,
}

/// One entry in a remote directory listing (or a single `Stat` result).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SftpEntry {
    pub name: String,
    pub kind: SftpEntryKind,
    #[serde(default)]
    pub size: u64,
    /// Modification time in whole seconds since the Unix epoch (0 if unknown).
    #[serde(default)]
    pub mtime: u64,
    /// Unix mode bits (permissions + type), 0 if the server didn't report them.
    #[serde(default)]
    pub permissions: u32,
    /// For a `Symlink`, whether the (followed) target is a directory — lets the
    /// GUI decide navigate-vs-download without another round-trip. Always false
    /// for non-symlinks.
    #[serde(default)]
    pub target_is_dir: bool,
}

/// A metadata / namespace operation on the remote filesystem. Recursive delete
/// (`RemoveDir`) recurses daemon-side. `Stat`/`Readlink` return data in the
/// [`SftpOpResult`]; the rest just succeed or fail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SftpOp {
    /// Follow-symlink stat of a single path.
    Stat {
        path: String,
    },
    Mkdir {
        path: String,
    },
    /// Create a new empty file, failing if one already exists at `path`.
    CreateFile {
        path: String,
    },
    RemoveFile {
        path: String,
    },
    /// Recursive directory delete (daemon walks + removes children first).
    RemoveDir {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    /// Set the permission (mode) bits of `path`.
    Chmod {
        path: String,
        mode: u32,
    },
    /// Read a symlink's target path (returned as [`SftpOpResult::Link`]).
    Readlink {
        path: String,
    },
}

/// The reply to a [`SftpOp`]. `Done` for side-effecting ops; `Stat`/`Link` carry
/// the queried data; `Error` carries a human-readable failure reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SftpOpResult {
    Done,
    Stat(SftpEntry),
    Link(String),
    Error(String),
}

/// Transfer direction for a background SFTP job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SftpTransferKind {
    /// local → remote.
    Upload,
    /// remote → local.
    Download,
}

/// The recipe for a background transfer job. `local` is a path in the *daemon
/// process's* filesystem (same user); `remote` is an absolute remote path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SftpTransferSpec {
    pub pane_id: u64,
    pub kind: SftpTransferKind,
    pub local: PathBuf,
    pub remote: String,
    /// Recurse into directories (create dirs on the far side).
    #[serde(default)]
    pub recursive: bool,
}

/// Lifecycle state of a transfer job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SftpJobState {
    Running,
    Done,
    Error,
    Cancelled,
}

/// A snapshot of one transfer job's progress, returned by the poll-based
/// `SftpTransferList` request while the tray is visible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SftpJobProgress {
    pub job_id: u64,
    pub pane_id: u64,
    pub kind: SftpTransferKind,
    pub state: SftpJobState,
    /// The path currently being transferred (a leaf within a recursive job).
    #[serde(default)]
    pub current: String,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default)]
    pub bytes_total: u64,
    /// Populated only when `state == Error`.
    #[serde(default)]
    pub error: Option<String>,
    /// Display labels (the job's endpoints).
    #[serde(default)]
    pub local: String,
    #[serde(default)]
    pub remote: String,
}

/// Runtime status of a live managed forward, surfaced to the GUI per row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForwardStatus {
    /// The forward's listener (Local/Dynamic) or remote binding (Remote) is up.
    Listening,
    /// The forward failed to come up (bind conflict, remote request denied, …).
    /// The string is a human-readable reason with no secrets.
    Error(String),
}

/// One established managed forward on a native-SSH pane's connection (WS4). This
/// is the runtime counterpart of a [`SshForwardRule`]: it carries a daemon-issued
/// `id` (used to remove it), the pane it is attributed to (for per-pane listing),
/// the *resolved* bind port (a `bind_port` of 0 resolves to the OS-assigned port),
/// and a live `status`.
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

fn default_term() -> String {
    "xterm-256color".to_string()
}

fn default_true() -> bool {
    true
}

/// The fully-resolved recipe for one native SSH connection + shell. See the
/// module-level comment above for the trust/secret model.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeSshSpec {
    pub host: String,
    pub port: u16,
    pub user: String,

    pub auth_mode: SshAuthMode,
    /// Private-key paths to try, in order. `%h`/`%r` expand to host/user.
    #[serde(default)]
    pub identity_files: Vec<String>,
    #[serde(default)]
    pub agent_forward: bool,

    /// Cleartext password, pre-resolved by the GUI from the keychain. SECRET.
    #[serde(default)]
    pub password: Option<String>,
    /// Passphrases for encrypted identity files, keyed by identity-file path (as
    /// listed in `identity_files`). Pre-resolved by the GUI. SECRET.
    #[serde(default)]
    pub key_passphrases: Option<std::collections::HashMap<String, String>>,

    #[serde(default)]
    pub proxy: SshProxy,
    /// Jump host: the GUI resolves a profile reference into a nested spec, so a
    /// multi-level chain is a chain of `jump` boxes. The daemon opens a
    /// `direct-tcpip` channel on the (recursively established) jump connection and
    /// uses it as this connection's transport.
    #[serde(default)]
    pub jump: Option<Box<NativeSshSpec>>,

    /// Preconfigured forwards — carried only (WS4 establishes them).
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
    /// X11 forwarding — carried only (implementing X11 channels is deferred).
    #[serde(default)]
    pub x11: bool,

    #[serde(default = "default_term")]
    pub term: String,
    #[serde(default = "default_true")]
    pub verify_host_keys: bool,
    #[serde(default)]
    pub skip_banner: bool,
    /// Lines sent verbatim (each + `\n`) to the shell channel after it starts,
    /// sequentially, with no expect-logic.
    #[serde(default)]
    pub login_script: Vec<String>,

    /// UI labeling only — never affects connection behavior.
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub profile_id: Option<String>,
}

impl NativeSshSpec {
    /// A clone with all secrets stripped (`password`, `key_passphrases`), and the
    /// jump chain stripped recursively. This is the form that is safe to persist
    /// (e.g. in `core::session` for native-SSH pane respawn) — the daemon
    /// re-resolves secrets from the GUI/keychain on the next connect.
    #[allow(dead_code)] // consumed by WS6 when persisting native-SSH panes
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
    /// Redacts secrets so a spec can be logged. `password` / `key_passphrases`
    /// collapse to a presence marker; the nested `jump` spec redacts recursively
    /// through this same impl.
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
            .field("login_script", &self.login_script)
            .field("display_name", &self.display_name)
            .field("profile_id", &self.profile_id)
            .finish()
    }
}

/// One row for the "SSH → Known hosts" management view (WS3): a single trusted
/// (or revoked / CA) `known_hosts` entry. Hashed hosts surface their raw `|1|…`
/// field (the hash can't be reversed to a hostname). Listed daemon-side because
/// the daemon owns file access on the native path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownHostEntry {
    /// Raw host field as stored (`example.com`, `[h]:2222`, a comma list, or a
    /// `|1|salt|hash` hashed token).
    pub host: String,
    /// `"@cert-authority"` / `"@revoked"` when the line carries a marker.
    #[serde(default)]
    pub marker: Option<String>,
    /// Key algorithm string (`ssh-ed25519`, `ecdsa-sha2-nistp256`, …).
    pub key_type: String,
    /// `SHA256:…` fingerprint, or `"?"` for an entry whose blob doesn't parse.
    pub fingerprint_sha256: String,
    /// Stable identity used to delete this exact entry.
    pub id: KnownHostId,
}

/// Content-based identity of one `known_hosts` entry (host field + key type +
/// blob), so a delete survives unrelated edits between list and delete rather
/// than relying on a fragile line index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownHostId {
    pub host: String,
    pub key_type: String,
    pub keyblob: String,
}

/// One prompt in a keyboard-interactive challenge (RFC 4256).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KiPrompt {
    pub text: String,
    /// Whether the user's keystrokes should be echoed (false for passwords).
    pub echo: bool,
}

/// An interactive decision the daemon needs from the GUI during a native-SSH
/// spawn. Sent as `DaemonMsg::AuthPrompt` over the pane's own connection, before
/// any `Output`; the daemon blocks that auth/host-key step until the matching
/// `ClientMsg::AuthResponse` arrives (or a timeout fails it cleanly).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthPromptKind {
    Password {
        user: String,
        host: String,
    },
    KeyPassphrase {
        key_path: String,
        comment: String,
    },
    KeyboardInteractive {
        name: String,
        instructions: String,
        prompts: Vec<KiPrompt>,
    },
    HostKeyUnknown {
        host: String,
        port: u16,
        algorithm: String,
        fingerprint_sha256: String,
    },
    HostKeyChanged {
        host: String,
        port: u16,
        algorithm: String,
        fingerprint_sha256: String,
        old_fingerprint_sha256: String,
    },
    /// A server auth banner. Fire-and-forget: no response is expected or awaited.
    Banner {
        text: String,
    },
}

/// The GUI's reply to an [`AuthPromptKind`]. `Secret`/`Secrets` carry cleartext
/// (a password, a passphrase, or keyboard-interactive answers); the hand-written
/// `Debug` redacts them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthResponse {
    Secret(String),
    Secrets(Vec<String>),
    HostKeyDecision {
        accept: bool,
        remember: bool,
    },
    /// The user dismissed the prompt; the daemon fails the auth step cleanly.
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

/// Progress of a native-SSH spawn, sent as `DaemonMsg::SshStatus` so the GUI can
/// show a status line while the connection comes up (or explain a failure).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SshPhase {
    Connecting,
    Authenticating,
    Connected,
    Failed { reason: String },
}

/// Messages the GUI client sends to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    /// Create a new pane (spawn a shell) in `cwd`, sized to `size`. The daemon
    /// replies `Spawned`, then this connection becomes that pane's stream.
    /// `shell` overrides the daemon's default shell resolution (config →
    /// platform default) when the user picked one from the new-tab dropdown.
    Spawn {
        cwd: Option<PathBuf>,
        size: WinSize,
        shell: Option<ShellSpec>,
    },
    /// Bind this connection to an existing pane and (re)size it. The daemon
    /// replies with a `Snapshot` then live `Output`.
    Attach { pane_id: u64, size: WinSize },
    /// Raw bytes typed/pasted into the pane. Hot path — payload is verbatim.
    Input(Vec<u8>),
    /// The client's view changed size; resize the PTY (`SIGWINCH` to the child).
    Resize(WinSize),
    /// Disconnect from the pane without killing it (it keeps running detached).
    Detach,
    /// Terminate a pane's child and forget it.
    Kill { pane_id: u64 },
    /// Ask for the list of live panes (control connection).
    List,
    /// Shut the whole daemon down: hang up every pane's child, then exit the
    /// process. A control-connection message the GUI sends to force a fresh
    /// daemon — e.g. so a newly granted macOS permission (Full Disk Access) takes
    /// effect, which a long-lived daemon process can't otherwise see. Ends every
    /// running session, so the caller confirms with the user first.
    Shutdown,
    /// Ensure a local SSH port-forward exists for a loopback URL printed by a
    /// remote session in `pane_id`. Control-connection message; daemon replies
    /// with `LoopbackForward` or `Error`.
    EnsureLoopbackForward(LoopbackForwardRequest),
    /// Ask for the daemon's active SSH loopback port-forwards.
    ListLoopbackForwards,
    /// Close one active SSH loopback port-forward.
    CloseLoopbackForward(LoopbackForwardId),
    /// Create a new pane backed by the daemon's native russh session engine.
    /// Like `Spawn`, but the pane's byte source is an SSH shell channel rather
    /// than a local PTY. `spec` is fully self-contained (see [`NativeSshSpec`]).
    /// This connection then becomes that pane's stream, and also carries the
    /// interactive auth/host-key exchange (`AuthPrompt`/`AuthResponse`).
    SpawnNativeSsh {
        cwd: Option<PathBuf>,
        size: WinSize,
        spec: Box<NativeSshSpec>,
    },
    /// The GUI's reply to a `DaemonMsg::AuthPrompt` with a matching `request_id`.
    /// Delivered on the pane's own connection while its native-SSH spawn is still
    /// authenticating.
    AuthResponse {
        request_id: u64,
        response: AuthResponse,
    },
    /// List the OpenSSH `known_hosts` entries for the "SSH → Known hosts" settings
    /// section (control connection; daemon replies with `KnownHostsList`).
    ListKnownHosts,
    /// Delete one `known_hosts` entry, then reply with the refreshed list.
    DeleteKnownHost(KnownHostId),
    /// List a remote directory over the pane's SFTP session (control connection).
    /// Daemon replies `SftpEntries` or `Error`.
    SftpList { pane_id: u64, path: String },
    /// A one-shot SFTP filesystem operation (mkdir/remove/rename/chmod/stat/…) on
    /// the pane's SFTP session. Daemon replies `SftpOpResult`.
    SftpOp { pane_id: u64, op: SftpOp },
    /// Start a background upload/download job on the pane's SFTP session. Daemon
    /// replies `SftpTransferStarted { job_id }` (or `Error`).
    SftpTransferStart(SftpTransferSpec),
    /// Cancel a running transfer job. Daemon replies with the current
    /// `SftpTransferProgress` list.
    SftpTransferCancel { job_id: u64 },
    /// Poll the transfer jobs for a pane (the GUI polls while its tray is
    /// visible). Daemon replies with a `SftpTransferProgress` list.
    SftpTransferList { pane_id: u64 },
    /// Establish a new managed port-forward (Local/Remote/Dynamic) on the native-SSH
    /// pane `pane_id`'s connection (WS4). Control-connection message; the daemon
    /// replies with a `ForwardList` reflecting the pane's forwards after the add.
    AddForward { pane_id: u64, rule: SshForwardRule },
    /// Tear down one managed forward by its daemon-issued id. Control-connection
    /// message; the daemon replies with the pane's remaining `ForwardList`.
    RemoveForward { pane_id: u64, forward_id: u64 },
    /// Ask for the managed forwards attributed to `pane_id`. Control-connection
    /// message; the daemon replies with a `ForwardList`.
    ListForwards { pane_id: u64 },
}

/// Messages the daemon sends back to the GUI client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonMsg {
    /// Result of `Spawn`: the id of the freshly created pane.
    Spawned { pane_id: u64 },
    /// The geometry the pane's ring was recorded under (the PTY's current
    /// size), sent immediately before `Snapshot` so the client can size its
    /// local grid to match before replaying. Replaying at any other width
    /// mis-wraps history and lands relative cursor motion on the wrong rows.
    Size(WinSize),
    /// One-shot replay of the pane's byte ring, sent right after `Attach`/`Spawn`
    /// so the client's local emulator rebuilds the current screen + scrollback.
    Snapshot(Vec<u8>),
    /// Live PTY output tail. Hot path — payload is verbatim.
    Output(Vec<u8>),
    /// The foreground cwd, sniffed daemon-side from OSC 7 / proc lookup.
    Cwd(PathBuf),
    /// Shell prompt/command state, sniffed daemon-side from OSC 133.
    Prompt {
        active: bool,
        at_prompt: bool,
        last_exit: Option<i32>,
    },
    /// The pane's child exited; `code` is its status when known.
    Exited { code: Option<i32> },
    /// Reply to `List`.
    PaneList(Vec<PaneInfo>),
    /// The foreground remote context, or `None` when the pane is local / unknown.
    RemoteContext(Option<RemoteContext>),
    /// Reply to `EnsureLoopbackForward`.
    LoopbackForward(LoopbackForward),
    /// Reply to `ListLoopbackForwards` and `CloseLoopbackForward`.
    LoopbackForwardList(Vec<LoopbackForwardInfo>),
    /// A native-SSH spawn needs an interactive decision from the GUI (password,
    /// passphrase, keyboard-interactive answers, or a host-key confirmation).
    /// Sent before `Output` starts flowing; the daemon blocks the auth step until
    /// a `ClientMsg::AuthResponse` with the same `request_id` arrives. A `Banner`
    /// prompt is fire-and-forget (no response awaited).
    AuthPrompt {
        request_id: u64,
        prompt: AuthPromptKind,
    },
    /// Progress of a native-SSH spawn (connect/auth/connected/failed).
    SshStatus { phase: SshPhase },
    /// Reply to `ListKnownHosts` and `DeleteKnownHost`.
    KnownHostsList(Vec<KnownHostEntry>),
    /// Reply to `SftpList`: the directory's entries (unsorted; the GUI sorts).
    SftpEntries(Vec<SftpEntry>),
    /// Reply to `SftpOp`.
    SftpOpResult(SftpOpResult),
    /// Reply to `SftpTransferStart`: the id of the freshly created job.
    SftpTransferStarted { job_id: u64 },
    /// Reply to `SftpTransferList` / `SftpTransferCancel`: progress snapshots.
    SftpTransferProgress(Vec<SftpJobProgress>),
    /// Reply to `AddForward` / `RemoveForward` / `ListForwards`: the managed
    /// forwards currently attributed to the requested pane (WS4).
    ForwardList(Vec<ManagedForward>),
    /// A request failed (e.g. `Attach` to an unknown/dead pane id).
    Error(String),
}

// Kind bytes. Client and daemon have independent spaces (a connection always
// knows which direction it is reading), so the small overlaps are intentional.
mod kind {
    // Client -> daemon
    pub const SPAWN: u8 = 1;
    pub const ATTACH: u8 = 2;
    pub const INPUT: u8 = 3;
    pub const RESIZE: u8 = 4;
    pub const DETACH: u8 = 5;
    pub const KILL: u8 = 6;
    pub const LIST: u8 = 7;
    pub const SHUTDOWN: u8 = 8;
    /// `Spawn` with an explicit, non-managed shell override. A separate kind
    /// (rather than a new field under `SPAWN`) so a default spawn stays
    /// byte-identical on the wire: the GUI and the long-lived daemon can be
    /// different versions, and an old daemon must keep serving new-GUI default
    /// spawns.
    pub const SPAWN_SHELL: u8 = 9;
    pub const ENSURE_LOOPBACK_FORWARD: u8 = 10;
    pub const LIST_LOOPBACK_FORWARDS: u8 = 11;
    pub const CLOSE_LOOPBACK_FORWARD: u8 = 12;
    // 13 (was `SPAWN_MANAGED_SSH`, the system-ssh compat funnel) is retired: all
    // SSH goes through the native russh engine (`SPAWN_NATIVE_SSH`).
    /// `SpawnNativeSsh` — the native russh session engine. A brand-new kind, so a
    /// daemon that predates WS2 rejects it (unknown kind → error) rather than
    /// mis-spawning; a native-SSH pane must never silently fall back to anything.
    pub const SPAWN_NATIVE_SSH: u8 = 14;
    /// `AuthResponse` — the GUI's reply to an `AUTH_PROMPT`.
    pub const AUTH_RESPONSE: u8 = 15;
    /// `ListKnownHosts` — control request for the known_hosts management view.
    pub const LIST_KNOWN_HOSTS: u8 = 16;
    /// `DeleteKnownHost` — remove one known_hosts entry.
    pub const DELETE_KNOWN_HOST: u8 = 17;
    // (WS3 reserves 15-17, WS4 reserves 20-24.) SFTP (WS5) owns 30-36.
    pub const SFTP_LIST: u8 = 30;
    pub const SFTP_OP: u8 = 31;
    pub const SFTP_TRANSFER_START: u8 = 32;
    pub const SFTP_TRANSFER_CANCEL: u8 = 33;
    pub const SFTP_TRANSFER_LIST: u8 = 34;
    // (16–19 reserved: WS3 auth extensions.)
    /// `AddForward` — establish a managed port-forward (WS4).
    pub const ADD_FORWARD: u8 = 20;
    /// `RemoveForward` — tear down one managed forward by id (WS4).
    pub const REMOVE_FORWARD: u8 = 21;
    /// `ListForwards` — list a pane's managed forwards (WS4).
    pub const LIST_FORWARDS: u8 = 22;

    // Daemon -> client
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
    /// `AuthPrompt` — an interactive auth/host-key request during a native-SSH spawn.
    pub const AUTH_PROMPT: u8 = 13;
    /// `SshStatus` — native-SSH spawn progress.
    pub const SSH_STATUS: u8 = 14;
    /// `KnownHostsList` — reply to `LIST_KNOWN_HOSTS` / `DELETE_KNOWN_HOST`.
    pub const KNOWN_HOSTS_LIST: u8 = 15;
    // SFTP (WS5) replies own 30-36 in the daemon space too.
    pub const SFTP_ENTRIES: u8 = 30;
    pub const SFTP_OP_RESULT: u8 = 31;
    pub const SFTP_TRANSFER_STARTED: u8 = 32;
    pub const SFTP_TRANSFER_PROGRESS: u8 = 33;
    // (15–19 reserved: WS3 auth extensions.)
    /// `ForwardList` — reply to the WS4 managed-forward messages.
    pub const FORWARD_LIST: u8 = 20;
}

/// Write one framed message: `[u32 LE len][u8 kind][payload]`.
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

/// Read one framed message, returning `(kind, payload)`. Returns an `UnexpectedEof`
/// error when the peer closes cleanly between frames (callers treat that as a
/// normal disconnect).
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

/// Extract one complete frame from the front of `buf`, if fully buffered — the
/// resumable counterpart of [`read_frame`] for callers that read the stream
/// with timeouts (the client reader enforces the DEC 2026 synchronized-update
/// deadline this way). A partial frame stays in `buf` untouched until more
/// bytes arrive, so a read that times out mid-frame loses nothing. Returns
/// `Ok(None)` while the frame is incomplete; an oversize length is a protocol
/// desync and errors, mirroring `read_frame`.
pub fn take_frame(buf: &mut Vec<u8>) -> io::Result<Option<(u8, Vec<u8>)>> {
    const HEADER: usize = 5; // u32 LE payload length + u8 kind
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

/// Serialize a control struct to JSON, mapping serde errors to `io::Error` so
/// the encode/decode surface is a single error type.
fn to_json<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn from_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> io::Result<T> {
    serde_json::from_slice(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

impl ClientMsg {
    /// Encode and write this message as one frame.
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        match self {
            // Default spawn keeps the legacy frame (kind + tuple payload)
            // byte-for-byte so an older daemon still serves it; an explicit
            // shell rides the newer SPAWN_SHELL frame. See `kind::SPAWN_SHELL`.
            ClientMsg::Spawn {
                cwd,
                size,
                shell: None,
            } => write_frame(w, kind::SPAWN, &to_json(&(cwd, size))?),
            ClientMsg::Spawn {
                cwd,
                size,
                shell: shell @ Some(_),
            } => write_frame(w, kind::SPAWN_SHELL, &to_json(&(cwd, size, shell))?),
            ClientMsg::Attach { pane_id, size } => {
                write_frame(w, kind::ATTACH, &to_json(&(pane_id, size))?)
            }
            ClientMsg::Input(bytes) => write_frame(w, kind::INPUT, bytes),
            ClientMsg::Resize(size) => write_frame(w, kind::RESIZE, &to_json(size)?),
            ClientMsg::Detach => write_frame(w, kind::DETACH, &[]),
            ClientMsg::Kill { pane_id } => write_frame(w, kind::KILL, &to_json(pane_id)?),
            ClientMsg::List => write_frame(w, kind::LIST, &[]),
            ClientMsg::Shutdown => write_frame(w, kind::SHUTDOWN, &[]),
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
            ClientMsg::ListForwards { pane_id } => {
                write_frame(w, kind::LIST_FORWARDS, &to_json(pane_id)?)
            }
        }
    }

    /// Reconstruct a message from a decoded frame.
    pub fn from_frame(k: u8, payload: Vec<u8>) -> io::Result<Self> {
        Ok(match k {
            kind::SPAWN => {
                let (cwd, size) = from_json(&payload)?;
                ClientMsg::Spawn {
                    cwd,
                    size,
                    shell: None,
                }
            }
            kind::SPAWN_SHELL => {
                let (cwd, size, shell) = from_json(&payload)?;
                ClientMsg::Spawn { cwd, size, shell }
            }
            kind::ATTACH => {
                let (pane_id, size) = from_json(&payload)?;
                ClientMsg::Attach { pane_id, size }
            }
            kind::INPUT => ClientMsg::Input(payload),
            kind::RESIZE => ClientMsg::Resize(from_json(&payload)?),
            kind::DETACH => ClientMsg::Detach,
            kind::KILL => ClientMsg::Kill {
                pane_id: from_json(&payload)?,
            },
            kind::LIST => ClientMsg::List,
            kind::SHUTDOWN => ClientMsg::Shutdown,
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
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown ClientMsg kind {other}"),
                ));
            }
        })
    }

    /// Read and decode the next client message from `r`.
    pub fn read<R: Read>(r: &mut R) -> io::Result<Self> {
        let (k, payload) = read_frame(r)?;
        Self::from_frame(k, payload)
    }
}

impl DaemonMsg {
    /// Encode and write this message as one frame.
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        match self {
            DaemonMsg::Spawned { pane_id } => write_frame(w, kind::SPAWNED, &to_json(pane_id)?),
            DaemonMsg::Size(size) => write_frame(w, kind::SIZE, &to_json(size)?),
            DaemonMsg::Snapshot(bytes) => write_frame(w, kind::SNAPSHOT, bytes),
            DaemonMsg::Output(bytes) => write_frame(w, kind::OUTPUT, bytes),
            DaemonMsg::Cwd(path) => write_frame(w, kind::CWD, &to_json(path)?),
            DaemonMsg::Prompt {
                active,
                at_prompt,
                last_exit,
            } => write_frame(w, kind::PROMPT, &to_json(&(active, at_prompt, last_exit))?),
            DaemonMsg::Exited { code } => write_frame(w, kind::EXITED, &to_json(code)?),
            DaemonMsg::PaneList(list) => write_frame(w, kind::PANE_LIST, &to_json(list)?),
            DaemonMsg::RemoteContext(remote) => {
                write_frame(w, kind::REMOTE_CONTEXT, &to_json(remote)?)
            }
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
            DaemonMsg::Error(msg) => write_frame(w, kind::ERROR, &to_json(msg)?),
        }
    }

    /// Reconstruct a message from a decoded frame.
    pub fn from_frame(k: u8, payload: Vec<u8>) -> io::Result<Self> {
        Ok(match k {
            kind::SPAWNED => DaemonMsg::Spawned {
                pane_id: from_json(&payload)?,
            },
            kind::SIZE => DaemonMsg::Size(from_json(&payload)?),
            kind::SNAPSHOT => DaemonMsg::Snapshot(payload),
            kind::OUTPUT => DaemonMsg::Output(payload),
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
            kind::REMOTE_CONTEXT => DaemonMsg::RemoteContext(from_json(&payload)?),
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
            kind::ERROR => DaemonMsg::Error(from_json(&payload)?),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown DaemonMsg kind {other}"),
                ));
            }
        })
    }

    /// Read and decode the next daemon message from `r`.
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

    /// End-to-end: a full attach session's worth of `ClientMsg`s and `DaemonMsg`s
    /// crossing a *real* duplex stream (loopback TCP — the same transport shape the
    /// daemon uses on Windows, and close enough to the Unix socket to exercise the
    /// framing). Unlike the single-`Cursor` round-trips above, this drives both
    /// directions across a thread boundary with mixed, back-to-back frames, so it
    /// catches framing bugs that only surface when `read_frame` must reassemble a
    /// message split across TCP segments or sitting behind an unrelated one. This is
    /// the client↔daemon IPC seam the rest of the suite otherwise only tests in
    /// halves.
    #[test]
    fn full_session_round_trips_over_a_real_duplex_stream() {
        use std::io::Write;
        use std::net::{TcpListener, TcpStream};
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // A realistic exchange: the client spawns a pane, resizes, types a command
        // and detaches; the daemon acknowledges, replays a snapshot, streams output,
        // reports prompt state, then exit.
        let client_msgs = vec![
            ClientMsg::Spawn {
                cwd: Some(PathBuf::from("/work")),
                size: SIZE,
                shell: None,
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

        // Daemon end: accept, decode every client message, then stream the replies.
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

        // Client end: send every request, then decode every reply.
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

    /// Round-trip every `ClientMsg` variant through encode → read.
    #[test]
    fn client_roundtrip() {
        let msgs = vec![
            ClientMsg::Spawn {
                cwd: Some(PathBuf::from("/tmp/x")),
                size: SIZE,
                shell: None,
            },
            ClientMsg::Spawn {
                cwd: None,
                size: SIZE,
                shell: None,
            },
            ClientMsg::Spawn {
                cwd: Some(PathBuf::from("/tmp/x")),
                size: SIZE,
                shell: Some(ShellSpec {
                    program: "wsl.exe".into(),
                    args: vec!["--distribution".into(), "Ubuntu".into()],
                }),
            },
            ClientMsg::Attach {
                pane_id: 42,
                size: SIZE,
            },
            ClientMsg::Input(vec![0x1b, b'[', b'A', 0, 255]),
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

    /// Round-trip every `DaemonMsg` variant through encode → read.
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
            DaemonMsg::PaneList(vec![PaneInfo {
                pane_id: 3,
                cwd: Some(PathBuf::from("/x")),
                title: "zsh".into(),
                alive: true,
            }]),
            DaemonMsg::RemoteContext(Some(RemoteContext {
                kind: RemoteKind::Ssh,
                argv: vec!["ssh".into(), "-p".into(), "2222".into(), "dev".into()],
                target: "dev".into(),
            })),
            DaemonMsg::RemoteContext(None),
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

    /// Wire compatibility across GUI/daemon version skew, both directions:
    /// a default spawn (`shell: None`) must emit the *legacy* frame — kind
    /// `SPAWN` with a `(cwd, size)` tuple an old daemon can decode — and a
    /// hand-built legacy frame must decode with `shell: None`. Locks the
    /// compat contract documented on `kind::SPAWN_SHELL`.
    #[test]
    fn default_spawn_stays_wire_compatible_with_old_daemons() {
        // New client -> old daemon: encode and pick the frame apart.
        let msg = ClientMsg::Spawn {
            cwd: Some(PathBuf::from("/work")),
            size: SIZE,
            shell: None,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let (k, payload) = read_frame(&mut std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(k, kind::SPAWN, "default spawn must use the legacy kind");
        // An old daemon deserializes exactly a (cwd, size) tuple.
        let (cwd, size): (Option<PathBuf>, WinSize) = serde_json::from_slice(&payload).unwrap();
        assert_eq!(cwd, Some(PathBuf::from("/work")));
        assert_eq!(size, SIZE);

        // Old client -> new daemon: a hand-built legacy frame decodes to
        // `shell: None`.
        let legacy = serde_json::to_vec(&(Some(PathBuf::from("/old")), SIZE)).unwrap();
        let decoded = ClientMsg::from_frame(kind::SPAWN, legacy).unwrap();
        assert_eq!(
            decoded,
            ClientMsg::Spawn {
                cwd: Some(PathBuf::from("/old")),
                size: SIZE,
                shell: None,
            }
        );
    }

    /// An explicit-shell spawn rides the `SPAWN_SHELL` frame (not the legacy
    /// `SPAWN` kind), and round-trips through encode → decode.
    #[test]
    fn explicit_shell_spawn_uses_shell_kind() {
        let shell = ShellSpec {
            program: "fish".to_string(),
            args: vec!["-l".to_string()],
        };
        let msg = ClientMsg::Spawn {
            cwd: Some(PathBuf::from("/work")),
            size: SIZE,
            shell: Some(shell.clone()),
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
            }
        );
    }

    /// An empty-payload binary frame (e.g. an `Input([])`) still round-trips and
    /// an oversize length is rejected.
    #[test]
    fn frame_edges() {
        let mut buf = Vec::new();
        write_frame(&mut buf, 3, &[]).unwrap();
        let mut cursor = std::io::Cursor::new(&buf);
        assert_eq!(read_frame(&mut cursor).unwrap(), (3, vec![]));

        // A hand-rolled frame claiming a huge length must be rejected.
        let mut bad = Vec::new();
        bad.extend_from_slice(&(u32::MAX).to_le_bytes());
        bad.push(3);
        let mut cursor = std::io::Cursor::new(&bad);
        assert!(read_frame(&mut cursor).is_err());
    }

    /// `write_frame` refuses to emit a payload larger than `MAX_FRAME` rather than
    /// putting a frame on the wire the peer would reject.
    #[test]
    fn write_frame_rejects_oversize_payload() {
        let oversize = vec![0u8; MAX_FRAME + 1];
        let mut buf = Vec::new();
        assert!(write_frame(&mut buf, 3, &oversize).is_err());
        // Nothing partial should have been emitted before the size check.
        assert!(buf.is_empty());
    }

    /// An unknown kind byte is a protocol desync, surfaced as an error (not a panic)
    /// for both directions.
    #[test]
    fn from_frame_rejects_unknown_kind() {
        assert!(ClientMsg::from_frame(99, vec![]).is_err());
        assert!(DaemonMsg::from_frame(99, vec![]).is_err());
    }

    /// `take_frame` decodes exactly `write_frame`'s output, leaves partial
    /// frames buffered (byte-at-a-time arrival included), preserves trailing
    /// bytes of the next frame, and rejects an oversize length.
    #[test]
    fn take_frame_is_resumable_and_mirrors_read_frame() {
        // Two frames, delivered one byte at a time: nothing decodes until each
        // frame completes, and the buffer is never corrupted by partial reads.
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

        // A complete frame followed by a partial one: the first pops, the
        // partial tail stays intact for the next read.
        let mut buf = Vec::new();
        write_frame(&mut buf, 3, b"done").unwrap();
        buf.extend_from_slice(&10u32.to_le_bytes()); // next frame's header only
        assert_eq!(take_frame(&mut buf).unwrap(), Some((3, b"done".to_vec())));
        assert_eq!(take_frame(&mut buf).unwrap(), None);
        assert_eq!(buf, 10u32.to_le_bytes());

        // An oversize length is a desync, same as read_frame.
        let mut bad = (u32::MAX).to_le_bytes().to_vec();
        bad.push(3);
        assert!(take_frame(&mut bad).is_err());
    }

    /// A frame truncated mid-stream — after the length prefix, or mid-payload —
    /// surfaces as an error (the reader treats it as a dropped peer), never a
    /// short/garbage frame.
    #[test]
    fn read_frame_on_truncated_frame_is_an_error() {
        // Length prefix only, no kind byte.
        let mut cut = std::io::Cursor::new(5u32.to_le_bytes().to_vec());
        assert_eq!(
            read_frame(&mut cut).unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );

        // Kind present but the payload is shorter than the length promised.
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

    /// A control frame whose JSON payload is garbage decodes to an error rather
    /// than panicking — a desynced peer can't crash the reader.
    #[test]
    fn from_frame_rejects_malformed_json_payloads() {
        assert!(ClientMsg::from_frame(kind::SPAWN, b"not json".to_vec()).is_err());
        assert!(DaemonMsg::from_frame(kind::PANE_LIST, b"{oops".to_vec()).is_err());
    }

    /// A clean close between frames (empty input) reads as `UnexpectedEof`, which
    /// callers treat as a normal disconnect.
    #[test]
    fn read_frame_on_empty_input_is_eof() {
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        let err = read_frame(&mut empty).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
        // The typed readers surface the same EOF.
        let mut empty2 = std::io::Cursor::new(Vec::<u8>::new());
        assert!(ClientMsg::read(&mut empty2).is_err());
    }

    /// `PaneInfo`'s `#[serde(default)]` fields tolerate an older/leaner JSON that
    /// omits `cwd` and `title`.
    #[test]
    fn pane_info_deserializes_with_defaults() {
        let info: PaneInfo = serde_json::from_str(r#"{"pane_id": 5, "alive": true}"#).unwrap();
        assert_eq!(info.pane_id, 5);
        assert!(info.alive);
        assert_eq!(info.cwd, None);
        assert_eq!(info.title, "");
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
            login_script: vec!["tmux attach".into()],
            display_name: Some("prod-web".into()),
            profile_id: Some("uuid-1".into()),
        }
    }

    /// The wire spec round-trips through serde, secrets and jump chain included.
    #[test]
    fn native_ssh_spec_serde_round_trips() {
        let spec = sample_native_spec();
        let json = serde_json::to_string(&spec).unwrap();
        let back: NativeSshSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
    }

    /// Missing optional fields decode via `#[serde(default)]` (forward compat).
    #[test]
    fn native_ssh_spec_tolerates_minimal_json() {
        let spec: NativeSshSpec =
            serde_json::from_str(r#"{"host":"h","port":22,"user":"u","auth_mode":"auto"}"#)
                .unwrap();
        assert_eq!(spec.term, "xterm-256color"); // defaulted
        assert!(spec.verify_host_keys); // defaulted true
        assert_eq!(spec.password, None);
        assert!(spec.jump.is_none());
    }

    /// The hand-written `Debug` must never leak secrets — for the spec *or* its
    /// nested jump spec — and `AuthResponse::Secret(s)` redact too.
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

    /// `without_secrets` clears passwords/passphrases recursively but keeps
    /// everything else, so the sanitized spec is safe to persist.
    #[test]
    fn without_secrets_strips_password_and_passphrases_recursively() {
        let clean = sample_native_spec().without_secrets();
        assert_eq!(clean.password, None);
        assert!(clean.key_passphrases.is_none());
        assert_eq!(clean.jump.as_ref().unwrap().password, None);
        // Non-secret fields survive.
        assert_eq!(clean.host, "example.com");
        assert_eq!(clean.login_script, vec!["tmux attach".to_string()]);
    }

    /// The new native-SSH client/daemon message variants round-trip through the
    /// frame codec (new kind bytes included).
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

    /// The native-SSH spawn uses a brand-new kind byte, so a pre-WS2 daemon
    /// rejects it (unknown kind) rather than mis-spawning.
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
}

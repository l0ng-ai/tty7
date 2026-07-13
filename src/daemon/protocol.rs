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
    /// When present, this pane is a tty7-owned SSH session. The daemon injects
    /// a ControlMaster/ControlPath at spawn time and later reuses that master for
    /// loopback forwards; ordinary shell picks leave this empty.
    #[serde(default)]
    pub ssh: Option<SshSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshSpec {
    /// The destination token (`host`, `user@host`, or ssh config alias).
    pub target: String,
    /// SSH client options that are safe to reuse for the master connection.
    #[serde(default)]
    pub args: Vec<String>,
}

impl SshSpec {
    pub fn validate(&self) -> Result<(), String> {
        let target = self.target.trim();
        if target.is_empty() {
            return Err("ssh target is empty".to_string());
        }
        // The target is appended as the trailing ssh argument without a `--`
        // separator, so a value starting with `-` would be parsed as an option
        // (e.g. `-oProxyCommand=…`, which ssh runs through a shell). ssh itself
        // rejects such destinations; refuse them here too rather than hand ssh
        // an option in destination position.
        if target.starts_with('-') {
            return Err("ssh target must not start with '-'".to_string());
        }
        validate_managed_ssh_args(&self.args)
    }
}

fn validate_managed_ssh_args(args: &[String]) -> Result<(), String> {
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            return Err("ssh options must not include a remote command".to_string());
        }
        if arg == "-N" || arg == "-f" || arg == "-T" {
            return Err(format!(
                "ssh option {arg} is not supported for managed SSH tabs"
            ));
        }
        if matches!(arg.as_str(), "-W" | "-w" | "-L" | "-R" | "-D" | "-S" | "-O") {
            return Err(format!(
                "ssh option {arg} conflicts with tty7-managed forwarding"
            ));
        }
        if arg == "-o" {
            i += 1;
            let Some(value) = args.get(i) else {
                return Err("ssh -o requires an option value".to_string());
            };
            let (name, _) = split_ssh_option(value);
            if managed_ssh_option_is_blocked(name) {
                return Err(format!(
                    "ssh option {name} conflicts with tty7-managed forwarding"
                ));
            }
            i += 1;
            continue;
        }
        if let Some(value) = arg.strip_prefix("-o")
            && !value.is_empty()
        {
            let value = value.strip_prefix('=').unwrap_or(value);
            let (name, _) = split_ssh_option(value);
            if managed_ssh_option_is_blocked(name) {
                return Err(format!(
                    "ssh option {name} conflicts with tty7-managed forwarding"
                ));
            }
            i += 1;
            continue;
        }
        if let Some(short) = arg.strip_prefix('-')
            && !short.starts_with('-')
            && short.len() > 1
        {
            let mut chars = short.chars();
            let Some(flag) = chars.next() else {
                return Err("empty ssh option".to_string());
            };
            if matches!(
                flag,
                'W' | 'w' | 'L' | 'R' | 'D' | 'S' | 'O' | 'N' | 'f' | 'T'
            ) {
                return Err(format!(
                    "ssh option -{flag} conflicts with tty7-managed forwarding"
                ));
            }
            if ssh_option_takes_value(flag) && chars.as_str().is_empty() {
                i += 1;
                if i >= args.len() {
                    return Err(format!("ssh option -{flag} requires a value"));
                }
            }
            i += 1;
            continue;
        }
        if arg.starts_with("--") {
            return Err(format!(
                "long ssh option {arg} is not supported for managed SSH tabs"
            ));
        }
        if !arg.starts_with('-') {
            return Err("ssh target must be entered separately from ssh options".to_string());
        }
        if arg.len() == 2 {
            let flag = arg.as_bytes()[1] as char;
            if ssh_option_takes_value(flag) {
                i += 1;
                if i >= args.len() {
                    return Err(format!("ssh option {arg} requires a value"));
                }
            }
        }
        i += 1;
    }
    Ok(())
}

fn split_ssh_option(value: &str) -> (&str, &str) {
    value
        .split_once('=')
        .map(|(name, value)| (name, value))
        .unwrap_or((value, ""))
}

fn managed_ssh_option_is_blocked(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "controlmaster"
            | "controlpath"
            | "controlpersist"
            | "exitonforwardfailure"
            | "forkafterauthentication"
            | "localforward"
            | "remoteforward"
            | "dynamicforward"
            | "permitlocalcommand"
            | "proxycommand"
            | "sessiontype"
            | "requesttty"
    )
}

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
    /// tty7-owned SSH master socket for this pane. Present only for managed SSH
    /// panes created through tty7; absent for process-table-detected foreground
    /// ssh commands typed inside a normal shell.
    #[serde(default)]
    pub control_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteKind {
    Ssh,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// `Spawn` with `ShellSpec.ssh`. It must not share `SPAWN_SHELL`: a daemon
    /// from before the `ssh` field existed would deserialize the rest of
    /// `ShellSpec`, ignore the unknown managed-SSH field, and silently launch a
    /// plain `ssh` tab with no ControlMaster/RemoteContext.
    pub const SPAWN_MANAGED_SSH: u8 = 13;

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
                shell: shell @ Some(spec),
            } => {
                let kind = if spec.ssh.is_some() {
                    kind::SPAWN_MANAGED_SSH
                } else {
                    kind::SPAWN_SHELL
                };
                write_frame(w, kind, &to_json(&(cwd, size, shell))?)
            }
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
            kind::SPAWN_SHELL | kind::SPAWN_MANAGED_SSH => {
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
                    ssh: None,
                }),
            },
            ClientMsg::Spawn {
                cwd: Some(PathBuf::from("/tmp/x")),
                size: SIZE,
                shell: Some(ShellSpec {
                    program: "ssh".into(),
                    args: vec!["-p".into(), "2222".into(), "dev".into()],
                    ssh: Some(SshSpec {
                        target: "dev".into(),
                        args: vec!["-p".into(), "2222".into()],
                    }),
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
    fn ssh_spec_allows_connection_options() {
        SshSpec {
            target: "dev".into(),
            args: vec![
                "-p".into(),
                "2222".into(),
                "-Jjump".into(),
                "-i".into(),
                "~/.ssh/id_ed25519".into(),
                "-o".into(),
                "UserKnownHostsFile=/tmp/known_hosts".into(),
            ],
        }
        .validate()
        .unwrap();
    }

    #[test]
    fn ssh_spec_rejects_forward_and_control_options() {
        for args in [
            vec!["-L".to_string(), "127.0.0.1:1:127.0.0.1:1".to_string()],
            vec!["-S".to_string(), "/tmp/other.sock".to_string()],
            vec!["-O".to_string(), "forward".to_string()],
            vec!["-o".to_string(), "ControlPath=/tmp/other.sock".to_string()],
            vec!["-oControlPath=/tmp/other.sock".to_string()],
            vec![
                "-o".to_string(),
                "LocalForward=127.0.0.1:1 127.0.0.1:1".to_string(),
            ],
            vec!["dev".to_string()],
        ] {
            assert!(
                SshSpec {
                    target: "dev".into(),
                    args: args.clone(),
                }
                .validate()
                .is_err(),
                "must reject {args:?}"
            );
        }
    }

    #[test]
    fn ssh_spec_rejects_option_like_target() {
        // A target starting with `-` is appended in destination position without
        // a `--` guard, so ssh would parse it as an option (e.g. an injected
        // `-oProxyCommand=…`). It must be refused regardless of surrounding
        // whitespace.
        for target in ["-oProxyCommand=touch /tmp/pwned", "-D8080", "  -N", "-"] {
            assert!(
                SshSpec {
                    target: target.into(),
                    args: vec![],
                }
                .validate()
                .is_err(),
                "must reject option-like target {target:?}"
            );
        }
        // A host that merely contains a dash elsewhere is fine.
        SshSpec {
            target: "my-host".into(),
            args: vec![],
        }
        .validate()
        .unwrap();
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
                control_path: Some(PathBuf::from("/tmp/tty7-ssh-1.sock")),
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

    #[test]
    fn managed_ssh_spawn_uses_non_legacy_kind() {
        let shell = ShellSpec {
            program: "ssh".to_string(),
            args: vec!["dev".to_string()],
            ssh: Some(SshSpec {
                target: "dev".to_string(),
                args: Vec::new(),
            }),
        };
        let msg = ClientMsg::Spawn {
            cwd: Some(PathBuf::from("/work")),
            size: SIZE,
            shell: Some(shell.clone()),
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        let (k, payload) = read_frame(&mut std::io::Cursor::new(&buf)).unwrap();
        assert_eq!(
            k,
            kind::SPAWN_MANAGED_SSH,
            "managed SSH must not use SPAWN_SHELL, because older daemons ignore unknown ShellSpec fields"
        );
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
}

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::protocol::{MAX_FRAME, read_frame, write_frame};

pub const CONTROL_VERSION: u32 = 5;

const DIALECT_MARKER: &str = "speaks control v";

fn dialect_refusal(peer_build: &str, peer: u32, ours: u32) -> String {
    format!("control peer (build {peer_build}) {DIALECT_MARKER}{peer}, this build speaks v{ours}")
}

pub fn is_dialect_refusal(message: &str) -> bool {
    message.contains(DIALECT_MARKER)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialectRefusal {
    pub peer_build: String,
    pub peer: u32,
    pub ours: u32,
}

pub fn parse_dialect_refusal(message: &str) -> Option<DialectRefusal> {
    const OPEN: &str = "control peer (build ";
    const TAIL: &str = ", this build speaks v";

    let rest = &message[message.find(OPEN)? + OPEN.len()..];
    let (peer_build, rest) = rest.split_once(") ")?;
    let rest = rest.strip_prefix(DIALECT_MARKER)?;
    let (peer, rest) = rest.split_once(TAIL)?;
    let ours: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some(DialectRefusal {
        peer_build: peer_build.to_string(),
        peer: peer.parse().ok()?,
        ours: ours.parse().ok()?,
    })
}

pub fn server_instance() -> &'static str {
    static INSTANCE: OnceLock<String> = OnceLock::new();
    INSTANCE.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

pub fn server_started() -> Instant {
    static STARTED: OnceLock<Instant> = OnceLock::new();
    *STARTED.get_or_init(Instant::now)
}

pub const WATCH_BURST_CAP: usize = 1024;

pub const WATCH_COALESCE_WINDOW: Duration = Duration::from_millis(100);

pub mod kind {

    pub const HELLO: u8 = 60;
    pub const REQUEST: u8 = 61;
    pub const REQUEST_BLOB: u8 = 62;
    pub const CANCEL: u8 = 63;

    pub const HELLO_OK: u8 = 60;
    pub const RESPONSE: u8 = 61;
    pub const RESPONSE_BLOB: u8 = 62;
    pub const EVENT: u8 = 63;
}

pub const GIT_STREAM_CHUNK: usize = 64 * 1024;

pub const GIT_STREAM_CHUNK_MAX: usize = crate::daemon::protocol::MAX_FRAME / 2;

pub const GIT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

pub const MAX_CONCURRENT_GIT_STREAMS: usize = 8;

pub mod feature {
    pub const CONTROL: &str = "control";
    pub const HOST_RPC: &str = "host-rpc";
    pub const MACHINE_TREE: &str = "machine-tree";
    pub const STDIO_BRIDGE: &str = "stdio-bridge";
}

pub use crate::host::{Entry, MTime, Meta, Output, SearchHit};

pub use crate::core::shells::{DetectedShell, ShellInventory};

pub use crate::core::machine::{Axis, LayoutDelta, Machine, PaneSeed, Side, Tab, TabId};
pub use crate::core::session::WorkspaceId;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlRequest {
    Ping,

    ReadDir {
        dir: String,
        root: Option<String>,
    },
    Stat {
        path: String,
    },
    Exists {
        path: String,
    },
    Canonicalize {
        path: String,
    },
    ReadFile {
        path: String,
        max_bytes: u64,
    },
    Search {
        roots: Vec<String>,
        query: String,
        limit: u64,
        max_dirs: u64,
        show_hidden: bool,
    },

    WriteFile {
        path: String,
    },
    CreateFileNew {
        path: String,
    },
    CreateDir {
        path: String,
        recursive: bool,
    },
    Rename {
        from: String,
        to: String,
    },
    Remove {
        path: String,
        recursive: bool,
    },

    RepoRoot {
        path: String,
    },
    Git {
        cwd: String,
        args: Vec<String>,
    },
    GitStream {
        id: u64,
        cwd: String,
        args: Vec<String>,
    },

    Shells,

    WatchOpen {
        dirs: Vec<String>,
    },
    WatchSet {
        id: u64,
        dirs: Vec<String>,
    },
    WatchClose {
        id: u64,
    },

    WorkspaceAttach {
        id: String,
    },
    WorkspaceDetach {
        id: String,
    },

    GuiOpen {
        path: Option<String>,
    },

    MachineGet,
    WorkspaceTree {
        workspace: WorkspaceId,
    },
    WorkspaceCreate {
        name: Option<String>,
        #[serde(default)]
        workspace: Option<WorkspaceId>,
    },
    WorkspaceRename {
        workspace: WorkspaceId,
        name: Option<String>,
    },
    WorkspaceRemove {
        workspace: WorkspaceId,
    },
    WorkspaceTouch {
        workspace: WorkspaceId,
    },
    WorkspaceSetActiveTab {
        workspace: WorkspaceId,
        tab: TabId,
    },
    TabCreate {
        workspace: WorkspaceId,
        at: Option<u64>,
        pane: PaneSeed,
        #[serde(default)]
        tab: Option<TabId>,
    },
    TabClose {
        workspace: WorkspaceId,
        tab: TabId,
    },
    TabRename {
        workspace: WorkspaceId,
        tab: TabId,
        name: Option<String>,
    },
    TabMove {
        workspace: WorkspaceId,
        tab: TabId,
        to: u64,
    },
    TabSetGroup {
        workspace: WorkspaceId,
        tab: TabId,
        group: Option<String>,
    },
    PaneSplit {
        workspace: WorkspaceId,
        pane: u64,
        axis: Axis,
        ratio: f32,
        new: PaneSeed,
        first: bool,
    },
    PaneClose {
        workspace: WorkspaceId,
        pane: u64,
    },
    PaneSetRatio {
        workspace: WorkspaceId,
        tab: TabId,
        path: Vec<Side>,
        ratio: f32,
    },
    PaneMove {
        workspace: WorkspaceId,
        pane: u64,
        to: u64,
        axis: Axis,
        first: bool,
    },
    PaneReplace {
        workspace: WorkspaceId,
        old: u64,
        new: PaneSeed,
    },

    AgentStates,
    Routes,
    Status,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneAgentState {
    pub pane_id: u64,
    #[serde(default)]
    pub agent: Option<crate::core::cli_agent::CLIAgent>,
    pub state: crate::core::cli_agent::AgentSessionState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteInfo {
    pub key: String,
    pub kind: String,
    pub connected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub pid: u32,
    pub uptime_secs: u64,
    pub panes: u64,
    pub control_version: u32,
    pub protocol_version: u32,
    #[serde(default)]
    pub build: String,
    #[serde(default)]
    pub socket: String,
}

impl ControlRequest {
    pub fn deadline(&self) -> Duration {
        use ControlRequest::*;
        match self {
            Ping => Duration::from_secs(5),
            ReadDir { .. }
            | Stat { .. }
            | Exists { .. }
            | Canonicalize { .. }
            | RepoRoot { .. }
            | WatchOpen { .. }
            | WatchSet { .. }
            | WatchClose { .. } => Duration::from_secs(5),
            ReadFile { .. } | WriteFile { .. } => Duration::from_secs(30),
            CreateFileNew { .. } | CreateDir { .. } | Rename { .. } | Remove { .. } => {
                Duration::from_secs(10)
            }
            Git { .. } | GitStream { .. } | Search { .. } => Duration::from_secs(20),
            Shells => Duration::from_secs(20),
            WorkspaceAttach { .. } | WorkspaceDetach { .. } => Duration::from_secs(10),
            GuiOpen { .. } => Duration::from_secs(5),
            MachineGet
            | WorkspaceTree { .. }
            | WorkspaceCreate { .. }
            | WorkspaceRename { .. }
            | WorkspaceRemove { .. }
            | WorkspaceTouch { .. }
            | WorkspaceSetActiveTab { .. }
            | TabCreate { .. }
            | TabClose { .. }
            | TabRename { .. }
            | TabMove { .. }
            | TabSetGroup { .. }
            | PaneSplit { .. }
            | PaneClose { .. }
            | PaneSetRatio { .. }
            | PaneMove { .. }
            | PaneReplace { .. } => Duration::from_secs(10),
            AgentStates | Routes | Status => Duration::from_secs(5),
        }
    }

    pub fn takes_blob(&self) -> bool {
        matches!(self, ControlRequest::WriteFile { .. })
    }

    pub fn returns_blob(&self) -> bool {
        matches!(self, ControlRequest::ReadFile { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ControlReply {
    #[serde(rename = "ok")]
    Ok(ReplyOk),
    #[serde(rename = "err")]
    Err(WireError),
}

impl ControlReply {
    pub fn into_result(self) -> io::Result<ReplyOk> {
        match self {
            ControlReply::Ok(v) => Ok(v),
            ControlReply::Err(e) => Err(e.into_io()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyOk {
    Unit,
    Pong,
    Entries(Vec<Entry>),
    Meta(Meta),
    Bool(bool),
    Path(String),
    OptPath(Option<String>),
    FileMeta { meta: Meta },
    Hits(Vec<SearchHit>),
    Output(Output),
    WatchId(u64),
    Shells(ShellInventory),
    Attached { took_over_from: Option<String> },
    MachineTree(Box<Machine>),
    WorkspaceTree(Box<crate::core::machine::Workspace>),
    TabTree(Box<Tab>),
    Panes(Vec<u64>),
    AgentStates(Vec<PaneAgentState>),
    Routes(Vec<RouteInfo>),
    Status(ServerStatus),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub kind: WireErrorKind,
    pub msg: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireErrorKind {
    NotFound,
    PermissionDenied,
    AlreadyExists,
    InvalidInput,
    NotADirectory,
    IsADirectory,
    DirectoryNotEmpty,
    FileTooLarge,
    GitUnavailable,
    TimedOut,
    ConnectionReset,
    Other,
}

impl WireError {
    pub fn new(kind: WireErrorKind, msg: impl Into<String>) -> Self {
        WireError {
            kind,
            msg: msg.into(),
        }
    }

    pub fn from_io(e: &io::Error) -> Self {
        use io::ErrorKind as K;
        let kind = match e.kind() {
            K::NotFound => WireErrorKind::NotFound,
            K::PermissionDenied => WireErrorKind::PermissionDenied,
            K::AlreadyExists => WireErrorKind::AlreadyExists,
            K::InvalidInput | K::InvalidFilename => WireErrorKind::InvalidInput,
            K::NotADirectory => WireErrorKind::NotADirectory,
            K::IsADirectory => WireErrorKind::IsADirectory,
            K::DirectoryNotEmpty => WireErrorKind::DirectoryNotEmpty,
            K::FileTooLarge => WireErrorKind::FileTooLarge,
            K::TimedOut => WireErrorKind::TimedOut,
            K::ConnectionReset | K::BrokenPipe | K::UnexpectedEof => WireErrorKind::ConnectionReset,
            _ => WireErrorKind::Other,
        };
        WireError {
            kind,
            msg: e.to_string(),
        }
    }

    pub fn into_io(self) -> io::Error {
        io::Error::new(self.kind.to_io_kind(), self.msg)
    }
}

impl WireErrorKind {
    pub fn to_io_kind(self) -> io::ErrorKind {
        use io::ErrorKind as K;
        match self {
            WireErrorKind::NotFound | WireErrorKind::GitUnavailable => K::NotFound,
            WireErrorKind::PermissionDenied => K::PermissionDenied,
            WireErrorKind::AlreadyExists => K::AlreadyExists,
            WireErrorKind::InvalidInput => K::InvalidInput,
            WireErrorKind::NotADirectory => K::NotADirectory,
            WireErrorKind::IsADirectory => K::IsADirectory,
            WireErrorKind::DirectoryNotEmpty => K::DirectoryNotEmpty,
            WireErrorKind::FileTooLarge => K::FileTooLarge,
            WireErrorKind::TimedOut => K::TimedOut,
            WireErrorKind::ConnectionReset => K::ConnectionReset,
            WireErrorKind::Other => K::Other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlEvent {
    Watch {
        id: u64,
        paths: Vec<String>,
    },
    WatchOverflow {
        id: u64,
    },

    GitChunk {
        id: u64,
        #[serde(with = "crate::host::b64")]
        bytes: Vec<u8>,
    },
    GitEnd {
        id: u64,
        code: Option<i32>,
        failed: bool,
    },

    PaneExited {
        pane_id: u64,
        code: Option<i32>,
    },
    AgentStatus {
        pane_id: u64,
        json: serde_json::Value,
    },
    Preempted {
        workspace: String,
        by: String,
    },
    GuiOpen {
        path: Option<String>,
    },
    Layout {
        workspace: String,
        delta: LayoutDelta,
    },
    LayoutResync,
}

pub type EventObserver = Arc<dyn Fn(crate::host::HostId, ControlEvent) + Send + Sync>;

static EVENT_OBSERVER: Mutex<Option<EventObserver>> = Mutex::new(None);

pub fn set_event_observer(f: EventObserver) {
    if let Ok(mut slot) = EVENT_OBSERVER.lock() {
        *slot = Some(f);
    }
}

pub fn observe_event(host: crate::host::HostId, event: ControlEvent) {
    let observer = match EVENT_OBSERVER.lock() {
        Ok(slot) => slot.clone(),
        Err(_) => None,
    };
    match observer {
        Some(f) => f(host, event),
        None => log::trace!("control event with nobody to hear it: {event:?}"),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlHello {
    pub control_version: u32,
    pub workspace: Option<String>,
    pub client_token: String,
    pub client_hostname: String,
    #[serde(default)]
    pub gui: bool,
}

impl ControlHello {
    pub fn host_rpc(client_token: impl Into<String>, client_hostname: impl Into<String>) -> Self {
        ControlHello {
            control_version: CONTROL_VERSION,
            workspace: None,
            client_token: client_token.into(),
            client_hostname: client_hostname.into(),
            gui: false,
        }
    }

    pub fn gui(client_token: impl Into<String>, client_hostname: impl Into<String>) -> Self {
        ControlHello {
            gui: true,
            ..Self::host_rpc(client_token, client_hostname)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlHelloOk {
    pub control_version: u32,
    pub protocol_version: u32,
    pub build: String,
    pub separator: char,
    pub home: String,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub instance: String,
}

impl ControlHelloOk {
    pub fn has_feature(&self, name: &str) -> bool {
        self.features.iter().any(|f| f == name)
    }
}

const CONTROL_HEADER: usize = 12;

fn to_json<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn from_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> io::Result<T> {
    serde_json::from_slice(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn encode_body(req_id: u64, json: &[u8], blob: &[u8]) -> io::Result<Vec<u8>> {
    let json_n = u32::try_from(json.len())
        .map_err(|_| invalid("control JSON exceeds the u32 length prefix"))?;
    let mut out = Vec::with_capacity(CONTROL_HEADER + json.len() + blob.len());
    out.extend_from_slice(&req_id.to_le_bytes());
    out.extend_from_slice(&json_n.to_le_bytes());
    out.extend_from_slice(json);
    out.extend_from_slice(blob);
    Ok(out)
}

fn decode_body(payload: &[u8]) -> io::Result<(u64, &[u8], &[u8])> {
    if payload.len() < CONTROL_HEADER {
        return Err(invalid(format!(
            "control payload is {} bytes, shorter than the {CONTROL_HEADER}-byte header",
            payload.len()
        )));
    }
    let req_id = u64::from_le_bytes(payload[..8].try_into().unwrap());
    let json_n = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
    let json_end = CONTROL_HEADER
        .checked_add(json_n)
        .ok_or_else(|| invalid("control json_len overflows the payload offset"))?;
    if json_end > payload.len() {
        return Err(invalid(format!(
            "control json_len {json_n} runs past the {}-byte payload",
            payload.len()
        )));
    }
    Ok((
        req_id,
        &payload[CONTROL_HEADER..json_end],
        &payload[json_end..],
    ))
}

fn decode_body_exact<'a>(payload: &'a [u8], what: &str) -> io::Result<(u64, &'a [u8])> {
    let (req_id, json, blob) = decode_body(payload)?;
    if !blob.is_empty() {
        return Err(invalid(format!(
            "{what} carries {} trailing bytes; only the *_BLOB kinds may",
            blob.len()
        )));
    }
    Ok((req_id, json))
}

fn require_nonzero(req_id: u64, what: &str) -> io::Result<()> {
    if req_id == 0 {
        return Err(invalid(format!(
            "{what} used req_id 0, which is reserved for server pushes"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub enum ControlClientMsg {
    Hello(ControlHello),
    Request {
        req_id: u64,
        req: ControlRequest,
    },
    RequestBlob {
        req_id: u64,
        req: ControlRequest,
        blob: Vec<u8>,
    },
    Cancel {
        req_id: u64,
    },
}

impl ControlClientMsg {
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let (k, payload) = self.to_frame()?;
        write_frame(w, k, &payload)
    }

    pub fn to_frame(&self) -> io::Result<(u8, Vec<u8>)> {
        let (k, payload) = match self {
            ControlClientMsg::Hello(hello) => (kind::HELLO, to_json(hello)?),
            ControlClientMsg::Request { req_id, req } => {
                require_nonzero(*req_id, "CONTROL_REQUEST")?;
                (kind::REQUEST, encode_body(*req_id, &to_json(req)?, &[])?)
            }
            ControlClientMsg::RequestBlob { req_id, req, blob } => {
                require_nonzero(*req_id, "CONTROL_REQUEST_BLOB")?;
                (
                    kind::REQUEST_BLOB,
                    encode_body(*req_id, &to_json(req)?, blob)?,
                )
            }
            ControlClientMsg::Cancel { req_id } => {
                require_nonzero(*req_id, "CONTROL_CANCEL")?;
                (kind::CANCEL, encode_body(*req_id, &[], &[])?)
            }
        };
        if payload.len() > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "control frame of {} bytes exceeds the {MAX_FRAME}-byte limit",
                    payload.len()
                ),
            ));
        }
        Ok((k, payload))
    }

    pub fn from_frame(k: u8, payload: Vec<u8>) -> io::Result<Self> {
        match k {
            kind::HELLO => Ok(ControlClientMsg::Hello(from_json(&payload)?)),
            kind::REQUEST => {
                let (req_id, json) = decode_body_exact(&payload, "CONTROL_REQUEST")?;
                require_nonzero(req_id, "CONTROL_REQUEST")?;
                Ok(ControlClientMsg::Request {
                    req_id,
                    req: from_json(json)?,
                })
            }
            kind::REQUEST_BLOB => {
                let (req_id, json, blob) = decode_body(&payload)?;
                require_nonzero(req_id, "CONTROL_REQUEST_BLOB")?;
                Ok(ControlClientMsg::RequestBlob {
                    req_id,
                    req: from_json(json)?,
                    blob: blob.to_vec(),
                })
            }
            kind::CANCEL => {
                let (req_id, json) = decode_body_exact(&payload, "CONTROL_CANCEL")?;
                require_nonzero(req_id, "CONTROL_CANCEL")?;
                if !json.is_empty() {
                    return Err(invalid("CONTROL_CANCEL must carry an empty JSON head"));
                }
                Ok(ControlClientMsg::Cancel { req_id })
            }
            other => Err(invalid(format!("unknown ControlClientMsg kind {other}"))),
        }
    }

    pub fn read<R: Read>(r: &mut R) -> io::Result<Self> {
        let (k, payload) = read_frame(r)?;
        Self::from_frame(k, payload)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ControlServerMsg {
    HelloOk(ControlHelloOk),
    Response {
        req_id: u64,
        reply: ControlReply,
    },
    ResponseBlob {
        req_id: u64,
        reply: ControlReply,
        blob: Vec<u8>,
    },
    Event(ControlEvent),
}

impl ControlServerMsg {
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let (k, payload) = self.to_frame()?;
        write_frame(w, k, &payload)
    }

    pub fn to_frame(&self) -> io::Result<(u8, Vec<u8>)> {
        let (k, payload) = match self {
            ControlServerMsg::HelloOk(ok) => (kind::HELLO_OK, to_json(ok)?),
            ControlServerMsg::Response { req_id, reply } => {
                require_nonzero(*req_id, "CONTROL_RESPONSE")?;
                (kind::RESPONSE, encode_body(*req_id, &to_json(reply)?, &[])?)
            }
            ControlServerMsg::ResponseBlob {
                req_id,
                reply,
                blob,
            } => {
                require_nonzero(*req_id, "CONTROL_RESPONSE_BLOB")?;
                (
                    kind::RESPONSE_BLOB,
                    encode_body(*req_id, &to_json(reply)?, blob)?,
                )
            }
            ControlServerMsg::Event(event) => (kind::EVENT, encode_body(0, &to_json(event)?, &[])?),
        };
        if payload.len() > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "control frame of {} bytes exceeds the {MAX_FRAME}-byte limit",
                    payload.len()
                ),
            ));
        }
        Ok((k, payload))
    }

    pub fn from_frame(k: u8, payload: Vec<u8>) -> io::Result<Self> {
        match k {
            kind::HELLO_OK => Ok(ControlServerMsg::HelloOk(from_json(&payload)?)),
            kind::RESPONSE => {
                let (req_id, json) = decode_body_exact(&payload, "CONTROL_RESPONSE")?;
                require_nonzero(req_id, "CONTROL_RESPONSE")?;
                Ok(ControlServerMsg::Response {
                    req_id,
                    reply: from_json(json)?,
                })
            }
            kind::RESPONSE_BLOB => {
                let (req_id, json, blob) = decode_body(&payload)?;
                require_nonzero(req_id, "CONTROL_RESPONSE_BLOB")?;
                Ok(ControlServerMsg::ResponseBlob {
                    req_id,
                    reply: from_json(json)?,
                    blob: blob.to_vec(),
                })
            }
            kind::EVENT => {
                let (req_id, json) = decode_body_exact(&payload, "CONTROL_EVENT")?;
                if req_id != 0 {
                    return Err(invalid(format!(
                        "CONTROL_EVENT carried req_id {req_id}; pushes must use 0"
                    )));
                }
                Ok(ControlServerMsg::Event(from_json(json)?))
            }
            other => Err(invalid(format!("unknown ControlServerMsg kind {other}"))),
        }
    }

    pub fn read<R: Read>(r: &mut R) -> io::Result<Self> {
        let (k, payload) = read_frame(r)?;
        Self::from_frame(k, payload)
    }
}

pub const KEEPALIVE_PING_INTERVAL: Duration = Duration::from_secs(15);
pub const KEEPALIVE_IDLE_BEFORE_PING: Duration = Duration::from_secs(30);
pub const KEEPALIVE_DEAD_AFTER: Duration = Duration::from_secs(45);

pub const CLOSE_GRACE: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, PartialEq)]
pub struct ControlResponse {
    pub reply: ReplyOk,
    pub blob: Vec<u8>,
}

pub type EventSink = Box<dyn Fn(ControlEvent) + Send + Sync + 'static>;

pub trait LinkShutdown: Send + Sync + 'static {
    fn shutdown_link(&self) -> io::Result<()>;
}

impl LinkShutdown for std::net::TcpStream {
    fn shutdown_link(&self) -> io::Result<()> {
        self.shutdown(std::net::Shutdown::Both)
    }
}

#[cfg(unix)]
impl LinkShutdown for std::os::unix::net::UnixStream {
    fn shutdown_link(&self) -> io::Result<()> {
        self.shutdown(std::net::Shutdown::Both)
    }
}

pub struct ControlClient {
    inner: Arc<ClientInner>,
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
}

struct ClientInner {
    writer: Mutex<Box<dyn Write + Send>>,
    next_req_id: AtomicU64,
    pending: Mutex<HashMap<u64, SyncSender<ControlReply>>>,
    blobs: Mutex<HashMap<u64, Vec<u8>>>,
    connected: AtomicBool,
    last_inbound: Mutex<Instant>,
    hello: ControlHelloOk,
    shutdown: Option<Arc<dyn LinkShutdown>>,
    reader_done: Mutex<bool>,
    reader_exit: Condvar,
    link_down: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
}

impl ControlClient {
    pub fn connect<R, W>(
        r: R,
        w: W,
        hello: &ControlHello,
        events: EventSink,
    ) -> io::Result<ControlClient>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        Self::connect_with(r, w, None, hello, events)
    }

    pub fn over_tcp(
        sock: std::net::TcpStream,
        hello: &ControlHello,
        events: EventSink,
    ) -> io::Result<ControlClient> {
        let r = sock.try_clone()?;
        let closer: Arc<dyn LinkShutdown> = Arc::new(sock.try_clone()?);
        Self::connect_with(r, sock, Some(closer), hello, events)
    }

    #[cfg(unix)]
    pub fn over_unix(
        sock: std::os::unix::net::UnixStream,
        hello: &ControlHello,
        events: EventSink,
    ) -> io::Result<ControlClient> {
        let r = sock.try_clone()?;
        let closer: Arc<dyn LinkShutdown> = Arc::new(sock.try_clone()?);
        Self::connect_with(r, sock, Some(closer), hello, events)
    }

    pub fn connect_with<R, W>(
        mut r: R,
        mut w: W,
        shutdown: Option<Arc<dyn LinkShutdown>>,
        hello: &ControlHello,
        events: EventSink,
    ) -> io::Result<ControlClient>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        ControlClientMsg::Hello(hello.clone()).encode(&mut w)?;
        w.flush()?;

        let ok = match ControlServerMsg::read(&mut r)? {
            ControlServerMsg::HelloOk(ok) => ok,
            other => {
                return Err(invalid(format!(
                    "control peer answered the handshake with {other:?} instead of HELLO_OK"
                )));
            }
        };
        if ok.control_version != hello.control_version {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                dialect_refusal(&ok.build, ok.control_version, hello.control_version),
            ));
        }

        let inner = Arc::new(ClientInner {
            writer: Mutex::new(Box::new(w)),
            next_req_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            blobs: Mutex::new(HashMap::new()),
            connected: AtomicBool::new(true),
            last_inbound: Mutex::new(Instant::now()),
            hello: ok,
            shutdown,
            reader_done: Mutex::new(false),
            reader_exit: Condvar::new(),
            link_down: Mutex::new(Vec::new()),
        });

        let reader_inner = Arc::clone(&inner);
        let reader = std::thread::Builder::new()
            .name("tty7-control-reader".into())
            .spawn(move || reader_loop(reader_inner, r, events))?;

        Ok(ControlClient {
            inner,
            reader: Mutex::new(Some(reader)),
        })
    }

    pub fn hello(&self) -> &ControlHelloOk {
        &self.inner.hello
    }

    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::Acquire)
    }

    pub fn on_link_down<F: Fn() + Send + Sync + 'static>(&self, hook: F) {
        if let Ok(mut hooks) = self.inner.link_down.lock() {
            hooks.push(Box::new(hook));
        }
        if !self.is_connected() {
            self.inner.run_link_down();
        }
    }

    pub fn idle_for(&self) -> Duration {
        self.inner
            .last_inbound
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }

    pub fn call(&self, req: ControlRequest) -> io::Result<ReplyOk> {
        self.call_full(req, &[]).map(|r| r.reply)
    }

    pub fn call_with_blob(&self, req: ControlRequest, blob: &[u8]) -> io::Result<ReplyOk> {
        self.call_full(req, blob).map(|r| r.reply)
    }

    pub fn call_full(&self, req: ControlRequest, blob: &[u8]) -> io::Result<ControlResponse> {
        let deadline = req.deadline();
        self.call_with_deadline(req, blob, deadline)
    }

    pub fn call_with_deadline(
        &self,
        req: ControlRequest,
        blob: &[u8],
        deadline: Duration,
    ) -> io::Result<ControlResponse> {
        if !self.is_connected() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "control connection is down",
            ));
        }

        let req_id = self.inner.next_req_id.fetch_add(1, Ordering::Relaxed);
        log::debug!(target: "tty7::control", "#{req_id} {req:?}");
        let (tx, rx) = sync_channel(1);
        self.inner.pending()?.insert(req_id, tx);

        let msg = if blob.is_empty() && !req.takes_blob() {
            ControlClientMsg::Request { req_id, req }
        } else {
            ControlClientMsg::RequestBlob {
                req_id,
                req,
                blob: blob.to_vec(),
            }
        };

        if let Err(e) = self.inner.send(&msg) {
            self.inner.forget(req_id);
            return Err(e);
        }

        match rx.recv_timeout(deadline) {
            Ok(reply) => {
                let blob = self.inner.take_blob(req_id);
                reply
                    .into_result()
                    .map(|reply| ControlResponse { reply, blob })
            }
            Err(RecvTimeoutError::Timeout) => {
                self.inner.forget(req_id);
                let _ = self.inner.send(&ControlClientMsg::Cancel { req_id });
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("control request {req_id} timed out after {deadline:?}"),
                ))
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.inner.forget(req_id);
                Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "control connection closed while the request was in flight",
                ))
            }
        }
    }

    pub fn ping(&self) -> io::Result<()> {
        self.call(ControlRequest::Ping).map(|_| ())
    }

    pub fn close(&self) {
        self.inner
            .fail_all("control connection closed by this client");
        if let Some(closer) = &self.inner.shutdown
            && let Err(e) = closer.shutdown_link()
        {
            log::trace!("control link shutdown reported {e}");
        }

        let Ok(mut slot) = self.reader.lock() else {
            return;
        };
        let Some(handle) = slot.take() else { return };

        let reaped = match self.inner.reader_done.lock() {
            Ok(done) => self
                .inner
                .reader_exit
                .wait_timeout_while(done, CLOSE_GRACE, |done| !*done)
                .map(|(done, _)| *done)
                .unwrap_or(false),
            Err(_) => false,
        };
        if reaped {
            let _ = handle.join();
        } else {
            log::debug!(
                "control reader did not exit within {CLOSE_GRACE:?}; detaching it \
                 rather than blocking the caller"
            );
            drop(handle);
        }
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        self.close();
    }
}

impl std::fmt::Debug for ControlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let in_flight = self.inner.pending.lock().map(|p| p.len()).unwrap_or(0);
        f.debug_struct("ControlClient")
            .field("build", &self.inner.hello.build)
            .field("control_version", &self.inner.hello.control_version)
            .field("connected", &self.is_connected())
            .field("in_flight", &in_flight)
            .finish()
    }
}

impl ClientInner {
    fn pending(
        &self,
    ) -> io::Result<std::sync::MutexGuard<'_, HashMap<u64, SyncSender<ControlReply>>>> {
        self.pending
            .lock()
            .map_err(|_| io::Error::other("control request table was poisoned"))
    }

    fn send(&self, msg: &ControlClientMsg) -> io::Result<()> {
        let (k, payload) = msg.to_frame()?;

        let mut w = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("control writer was poisoned"))?;
        let r = write_frame(&mut *w, k, &payload).and_then(|()| w.flush());
        if r.is_err() {
            self.connected.store(false, Ordering::Release);
        }
        r
    }

    fn forget(&self, req_id: u64) {
        if let Ok(mut p) = self.pending.lock() {
            p.remove(&req_id);
        }
        if let Ok(mut b) = self.blobs.lock() {
            b.remove(&req_id);
        }
    }

    fn take_blob(&self, req_id: u64) -> Vec<u8> {
        self.blobs
            .lock()
            .ok()
            .and_then(|mut b| b.remove(&req_id))
            .unwrap_or_default()
    }

    fn deliver(&self, req_id: u64, reply: ControlReply, blob: Vec<u8>) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let Some(tx) = pending.remove(&req_id) else {
            log::trace!("control reply for unknown req_id {req_id}; dropping");
            return;
        };
        if !blob.is_empty()
            && let Ok(mut blobs) = self.blobs.lock()
        {
            blobs.insert(req_id, blob);
        }
        drop(pending);
        let _ = tx.try_send(reply);
    }

    fn fail_all(&self, why: &str) {
        self.connected.store(false, Ordering::Release);
        let drained: Vec<_> = match self.pending.lock() {
            Ok(mut p) => p.drain().collect(),
            Err(_) => Vec::new(),
        };
        for (req_id, tx) in drained {
            let _ = tx.try_send(ControlReply::Err(WireError::new(
                WireErrorKind::ConnectionReset,
                format!("{why} (request {req_id})"),
            )));
        }
        self.run_link_down();
    }

    fn run_link_down(&self) {
        if let Ok(hooks) = self.link_down.lock() {
            for hook in hooks.iter() {
                hook();
            }
        }
    }
}

fn reader_loop<R: Read>(inner: Arc<ClientInner>, r: R, events: EventSink) {
    read_until_closed(&inner, r, events);
    if let Ok(mut done) = inner.reader_done.lock() {
        *done = true;
    }
    inner.reader_exit.notify_all();
}

fn read_until_closed<R: Read>(inner: &Arc<ClientInner>, mut r: R, events: EventSink) {
    loop {
        let frame = match read_frame(&mut r) {
            Ok(f) => f,
            Err(e) => {
                log::debug!("control reader stopping: {e}");
                inner.fail_all("control connection lost");
                return;
            }
        };
        if let Ok(mut t) = inner.last_inbound.lock() {
            *t = Instant::now();
        }
        match ControlServerMsg::from_frame(frame.0, frame.1) {
            Ok(ControlServerMsg::Response { req_id, reply }) => {
                inner.deliver(req_id, reply, Vec::new());
            }
            Ok(ControlServerMsg::ResponseBlob {
                req_id,
                reply,
                blob,
            }) => {
                inner.deliver(req_id, reply, blob);
            }
            Ok(ControlServerMsg::Event(event)) => events(event),
            Ok(ControlServerMsg::HelloOk(_)) => {
                log::warn!("control peer sent a second HELLO_OK; dropping the connection");
                inner.fail_all("control peer re-sent its handshake");
                return;
            }
            Err(e) => {
                log::warn!("control decode error: {e}");
                inner.fail_all("control stream desynchronized");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::MAX_FRAME;
    use std::io::Cursor;
    use std::path::PathBuf;

    fn meta() -> Meta {
        Meta {
            is_dir: false,
            is_symlink: false,
            len: 4096,
            mtime: Some(MTime {
                secs: 1_769_000_000,
                nanos: 123_456_789,
            }),
            readonly: false,
        }
    }

    fn detached_inner() -> Arc<ClientInner> {
        Arc::new(ClientInner {
            writer: Mutex::new(Box::new(Cursor::new(Vec::new()))),
            next_req_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            blobs: Mutex::new(HashMap::new()),
            connected: AtomicBool::new(true),
            last_inbound: Mutex::new(Instant::now()),
            hello: ControlHelloOk {
                control_version: CONTROL_VERSION,
                protocol_version: 0,
                build: "test".into(),
                separator: '/',
                home: "/home/me".into(),
                features: Vec::new(),
                instance: "test-instance".into(),
            },
            shutdown: None,
            reader_done: Mutex::new(false),
            reader_exit: Condvar::new(),
            link_down: Mutex::new(Vec::new()),
        })
    }

    fn unrepresentable_path() -> PathBuf {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/caf\xe9.rs"))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt as _;
            PathBuf::from(std::ffi::OsString::from_wide(&[
                0x0043, 0x003a, 0x005c, 0xd800, 0x002e, 0x0072, 0x0073,
            ]))
        }
    }

    #[test]
    fn to_frame_refuses_what_cannot_be_sent_without_writing_it() {
        let unrepresentable = ControlServerMsg::Response {
            req_id: 1,
            reply: ControlReply::Ok(ReplyOk::Hits(vec![SearchHit {
                name: "caf\u{fffd}.rs".into(),
                path: unrepresentable_path(),
                is_dir: false,
                ignored: false,
            }])),
        };
        assert!(
            unrepresentable.to_frame().is_err(),
            "a path that is not valid Unicode must not encode"
        );

        let huge = ControlServerMsg::ResponseBlob {
            req_id: 1,
            reply: ControlReply::Ok(ReplyOk::Meta(meta())),
            blob: vec![0u8; MAX_FRAME + 1],
        };
        let err = huge
            .to_frame()
            .expect_err("an oversize reply must not encode");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_frame_too_large_to_send_does_not_condemn_the_link() {
        let inner = detached_inner();
        let oversize = ControlClientMsg::RequestBlob {
            req_id: 1,
            req: ControlRequest::WriteFile {
                path: "/home/me/huge.bin".into(),
            },
            blob: vec![0u8; MAX_FRAME + 1],
        };

        let err = inner
            .send(&oversize)
            .expect_err("an oversize frame must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            inner.connected.load(Ordering::Acquire),
            "a local encode failure marked the connection dead"
        );

        inner.send(&ControlClientMsg::Cancel { req_id: 1 }).unwrap();
    }

    #[test]
    fn a_blob_racing_its_callers_timeout_is_not_retained() {
        for req_id in 1..2_000u64 {
            let inner = detached_inner();
            let (tx, _rx) = std::sync::mpsc::sync_channel(1);
            inner.pending.lock().unwrap().insert(req_id, tx);

            let giving_up = Arc::clone(&inner);
            let t = std::thread::spawn(move || giving_up.forget(req_id));
            inner.deliver(req_id, ControlReply::Ok(ReplyOk::Unit), vec![7u8; 64]);
            t.join().unwrap();

            assert!(
                inner.blobs.lock().unwrap().is_empty(),
                "a blob outlived the request it belonged to (req_id {req_id})"
            );
        }
    }

    fn every_request() -> Vec<ControlRequest> {
        vec![
            ControlRequest::Ping,
            ControlRequest::ReadDir {
                dir: "/home/me/proj/src".into(),
                root: Some("/home/me/proj".into()),
            },
            ControlRequest::ReadDir {
                dir: "/tmp".into(),
                root: None,
            },
            ControlRequest::Stat {
                path: "/home/me/.zshrc".into(),
            },
            ControlRequest::Exists {
                path: "/nope".into(),
            },
            ControlRequest::Canonicalize {
                path: "/a/../b".into(),
            },
            ControlRequest::ReadFile {
                path: "/home/me/proj/Cargo.toml".into(),
                max_bytes: 10 * 1024 * 1024,
            },
            ControlRequest::Search {
                roots: vec!["/home/me/proj".into(), "/srv".into()],
                query: "widget".into(),
                limit: 200,
                max_dirs: 2000,
                show_hidden: false,
            },
            ControlRequest::WriteFile {
                path: "/home/me/proj/src/main.rs".into(),
            },
            ControlRequest::CreateFileNew {
                path: "/home/me/new.txt".into(),
            },
            ControlRequest::CreateDir {
                path: "/home/me/a/b/c".into(),
                recursive: true,
            },
            ControlRequest::Rename {
                from: "/a".into(),
                to: "/b".into(),
            },
            ControlRequest::Remove {
                path: "/home/me/tmp".into(),
                recursive: true,
            },
            ControlRequest::RepoRoot {
                path: "/home/me/proj/src/deep".into(),
            },
            ControlRequest::Git {
                cwd: "/home/me/proj".into(),
                args: vec!["status".into(), "--porcelain".into()],
            },
            ControlRequest::WatchOpen {
                dirs: vec!["/home/me/proj".into()],
            },
            ControlRequest::WatchSet {
                id: 7,
                dirs: vec!["/home/me/proj".into(), "/home/me/proj/src".into()],
            },
            ControlRequest::WatchClose { id: 7 },
            ControlRequest::GuiOpen {
                path: Some("/home/me/proj".into()),
            },
            ControlRequest::AgentStates,
            ControlRequest::Routes,
            ControlRequest::Status,
        ]
    }

    fn every_reply() -> Vec<ControlReply> {
        vec![
            ControlReply::Ok(ReplyOk::Unit),
            ControlReply::Ok(ReplyOk::Pong),
            ControlReply::Ok(ReplyOk::Entries(vec![
                Entry {
                    name: "src".into(),
                    is_dir: true,
                    is_symlink: false,
                    ignored: false,
                },
                Entry {
                    name: "target".into(),
                    is_dir: true,
                    is_symlink: false,
                    ignored: true,
                },
                Entry {
                    name: "link".into(),
                    is_dir: false,
                    is_symlink: true,
                    ignored: false,
                },
            ])),
            ControlReply::Ok(ReplyOk::Meta(meta())),
            ControlReply::Ok(ReplyOk::Bool(true)),
            ControlReply::Ok(ReplyOk::Bool(false)),
            ControlReply::Ok(ReplyOk::Path("/home/me/proj".into())),
            ControlReply::Ok(ReplyOk::OptPath(Some("/home/me/proj".into()))),
            ControlReply::Ok(ReplyOk::OptPath(None)),
            ControlReply::Ok(ReplyOk::FileMeta { meta: meta() }),
            ControlReply::Ok(ReplyOk::Hits(vec![SearchHit {
                name: "widget.rs".into(),
                path: PathBuf::from("/home/me/proj/src/widget.rs"),
                is_dir: false,
                ignored: false,
            }])),
            ControlReply::Ok(ReplyOk::Output(Output {
                status: Some(1),
                stdout: b"?? src/new.rs\n".to_vec(),
                stderr: vec![0x00, 0xff, 0xfe, b'\n'],
            })),
            ControlReply::Ok(ReplyOk::WatchId(42)),
            ControlReply::Ok(ReplyOk::AgentStates(vec![PaneAgentState {
                pane_id: 9,
                agent: Some(crate::core::cli_agent::CLIAgent::Claude),
                state: crate::core::cli_agent::AgentSessionState {
                    status: crate::core::cli_agent::AgentStatus::Waiting,
                    message: Some("needs permission".into()),
                    session_id: Some("sess-1".into()),
                    launch_argv: Some(vec!["claude".into()]),
                    rich: true,
                    cwd: Some("/work/api".into()),
                    activity: 3,
                },
            }])),
            ControlReply::Ok(ReplyOk::AgentStates(Vec::new())),
            ControlReply::Ok(ReplyOk::Routes(vec![RouteInfo {
                key: "me@build-box:22".into(),
                kind: "ssh".into(),
                connected: true,
            }])),
            ControlReply::Ok(ReplyOk::Status(ServerStatus {
                pid: 4242,
                uptime_secs: 61,
                panes: 3,
                control_version: CONTROL_VERSION,
                protocol_version: crate::daemon::protocol::PROTOCOL_VERSION,
                build: "26.7.5".into(),
                socket: "/run/user/1000/tty7/daemon.sock".into(),
            })),
            ControlReply::Err(WireError::new(WireErrorKind::NotFound, "no such file")),
            ControlReply::Err(WireError::new(
                WireErrorKind::PermissionDenied,
                "permission denied",
            )),
            ControlReply::Err(WireError::new(
                WireErrorKind::AlreadyExists,
                "already exists",
            )),
            ControlReply::Err(WireError::new(WireErrorKind::InvalidInput, "bad path")),
            ControlReply::Err(WireError::new(WireErrorKind::NotADirectory, "not a dir")),
            ControlReply::Err(WireError::new(WireErrorKind::IsADirectory, "is a dir")),
            ControlReply::Err(WireError::new(
                WireErrorKind::DirectoryNotEmpty,
                "not empty",
            )),
            ControlReply::Err(WireError::new(WireErrorKind::FileTooLarge, "too large")),
            ControlReply::Err(WireError::new(WireErrorKind::GitUnavailable, "no git")),
            ControlReply::Err(WireError::new(WireErrorKind::TimedOut, "timed out")),
            ControlReply::Err(WireError::new(WireErrorKind::ConnectionReset, "reset")),
            ControlReply::Err(WireError::new(WireErrorKind::Other, "something else")),
        ]
    }

    fn every_event() -> Vec<ControlEvent> {
        vec![
            ControlEvent::Watch {
                id: 3,
                paths: vec!["/home/me/proj/src/main.rs".into()],
            },
            ControlEvent::WatchOverflow { id: 3 },
            ControlEvent::PaneExited {
                pane_id: 9,
                code: Some(0),
            },
            ControlEvent::PaneExited {
                pane_id: 9,
                code: None,
            },
            ControlEvent::AgentStatus {
                pane_id: 9,
                json: serde_json::json!({ "state": "thinking" }),
            },
            ControlEvent::Preempted {
                workspace: "w1".into(),
                by: "other-laptop".into(),
            },
            ControlEvent::GuiOpen {
                path: Some("/home/me/proj".into()),
            },
        ]
    }

    fn hello() -> ControlHello {
        ControlHello {
            control_version: CONTROL_VERSION,
            workspace: Some("w1".into()),
            client_token: "tok".into(),
            client_hostname: "laptop".into(),
            gui: false,
        }
    }

    #[test]
    fn the_server_instance_is_one_value_per_process() {
        let first = server_instance();
        assert!(!first.is_empty(), "an empty instance means \"unknown\"");
        assert_eq!(first, server_instance());
    }

    #[test]
    fn the_server_start_instant_is_fixed_per_process() {
        let first = server_started();
        assert_eq!(first, server_started(), "uptime needs one fixed origin");
        assert!(first.elapsed() >= Duration::ZERO);
    }

    #[test]
    fn a_hello_without_an_instance_still_decodes() {
        let json = r#"{"control_version":2,"protocol_version":3,"build":"26.7.6",
                       "separator":"/","home":"/home/me"}"#;
        let ok: ControlHelloOk = serde_json::from_str(json).expect("decodes without instance");
        assert_eq!(ok.instance, "");
        assert!(ok.features.is_empty());
    }

    #[test]
    fn an_old_client_hello_defaults_to_a_non_gui_connection() {
        let json = r#"{"control_version":5,"workspace":null,"client_token":"tok",
                       "client_hostname":"laptop"}"#;
        let hello: ControlHello = serde_json::from_str(json).expect("decodes without gui role");
        assert!(!hello.gui);
    }

    fn hello_ok() -> ControlHelloOk {
        ControlHelloOk {
            control_version: CONTROL_VERSION,
            protocol_version: crate::daemon::protocol::PROTOCOL_VERSION,
            build: "26.7.5".into(),
            separator: '/',
            home: "/home/me".into(),
            features: vec![feature::CONTROL.into(), feature::HOST_RPC.into()],
            instance: "test-instance".into(),
        }
    }

    #[test]
    fn client_messages_round_trip() {
        let mut msgs = vec![
            ControlClientMsg::Hello(hello()),
            ControlClientMsg::Hello(ControlHello::host_rpc("tok", "laptop")),
            ControlClientMsg::Cancel { req_id: 1 },
            ControlClientMsg::Cancel { req_id: u64::MAX },
            ControlClientMsg::RequestBlob {
                req_id: 5,
                req: ControlRequest::WriteFile {
                    path: "/tmp/x".into(),
                },
                blob: vec![0x00, 0xff, b'h', b'i'],
            },
            ControlClientMsg::RequestBlob {
                req_id: 6,
                req: ControlRequest::WriteFile {
                    path: "/tmp/empty".into(),
                },
                blob: Vec::new(),
            },
        ];
        for (i, req) in every_request().into_iter().enumerate() {
            msgs.push(ControlClientMsg::Request {
                req_id: i as u64 + 1,
                req,
            });
        }

        for msg in &msgs {
            let mut buf = Vec::new();
            msg.encode(&mut buf).unwrap();
            let got = ControlClientMsg::read(&mut Cursor::new(&buf)).unwrap();
            assert_eq!(&got, msg, "client message did not survive the round trip");
        }
    }

    #[test]
    fn server_messages_round_trip() {
        let mut msgs = vec![
            ControlServerMsg::HelloOk(hello_ok()),
            ControlServerMsg::ResponseBlob {
                req_id: 9,
                reply: ControlReply::Ok(ReplyOk::FileMeta { meta: meta() }),
                blob: b"file contents\0\xff".to_vec(),
            },
            ControlServerMsg::ResponseBlob {
                req_id: 10,
                reply: ControlReply::Ok(ReplyOk::FileMeta { meta: meta() }),
                blob: Vec::new(),
            },
        ];
        for (i, reply) in every_reply().into_iter().enumerate() {
            msgs.push(ControlServerMsg::Response {
                req_id: i as u64 + 1,
                reply,
            });
        }
        for event in every_event() {
            msgs.push(ControlServerMsg::Event(event));
        }

        for msg in &msgs {
            let mut buf = Vec::new();
            msg.encode(&mut buf).unwrap();
            let got = ControlServerMsg::read(&mut Cursor::new(&buf)).unwrap();
            assert_eq!(&got, msg, "server message did not survive the round trip");
        }
    }

    #[test]
    fn frame_layout_matches_the_contract() {
        let mut buf = Vec::new();
        ControlClientMsg::RequestBlob {
            req_id: 0x0102_0304_0506_0708,
            req: ControlRequest::WriteFile { path: "/x".into() },
            blob: vec![0xaa, 0xbb],
        }
        .encode(&mut buf)
        .unwrap();

        let payload_len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        assert_eq!(buf[4], 62, "REQUEST_BLOB is kind 62");
        assert_eq!(
            buf.len(),
            5 + payload_len,
            "frame length covers the payload"
        );

        let payload = &buf[5..];
        assert_eq!(
            &payload[..8],
            &0x0102_0304_0506_0708u64.to_le_bytes(),
            "req_id is 8 bytes, little-endian, first"
        );
        let json_n = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
        let json = &payload[12..12 + json_n];
        assert_eq!(
            serde_json::from_slice::<ControlRequest>(json).unwrap(),
            ControlRequest::WriteFile { path: "/x".into() }
        );
        assert_eq!(&payload[12 + json_n..], &[0xaa, 0xbb], "blob is the tail");
        assert_eq!(
            payload_len,
            12 + json_n + 2,
            "payload_len == 12 + json_n + blob_len"
        );
    }

    #[test]
    fn non_blob_frames_have_no_tail() {
        let mut buf = Vec::new();
        ControlClientMsg::Request {
            req_id: 1,
            req: ControlRequest::Ping,
        }
        .encode(&mut buf)
        .unwrap();
        let payload_len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        let json_n = u32::from_le_bytes(buf[13..17].try_into().unwrap()) as usize;
        assert_eq!(payload_len, 12 + json_n);
    }

    #[test]
    fn cancel_is_twelve_bytes_with_an_empty_json_head() {
        let mut buf = Vec::new();
        ControlClientMsg::Cancel { req_id: 77 }
            .encode(&mut buf)
            .unwrap();
        assert_eq!(u32::from_le_bytes(buf[..4].try_into().unwrap()), 12);
        assert_eq!(buf[4], 63);
        assert_eq!(&buf[5..13], &77u64.to_le_bytes());
        assert_eq!(&buf[13..17], &0u32.to_le_bytes());
        assert_eq!(buf.len(), 17);
    }

    #[test]
    fn a_git_chunk_crosses_the_wire_as_base64() {
        let bytes: Vec<u8> = (0..8192u32).map(|i| b'a' + (i % 26) as u8).collect();
        let event = ControlEvent::GitChunk {
            id: 7,
            bytes: bytes.clone(),
        };
        let json = to_json(&event).unwrap();
        assert!(
            json.len() < bytes.len() * 2,
            "a {}-byte chunk encoded to {} bytes of JSON — the array-of-numbers \
             encoding is back",
            bytes.len(),
            json.len()
        );

        let (k, payload) = ControlServerMsg::Event(event).to_frame().unwrap();
        match ControlServerMsg::from_frame(k, payload).unwrap() {
            ControlServerMsg::Event(ControlEvent::GitChunk { id, bytes: got }) => {
                assert_eq!(id, 7);
                assert_eq!(got, bytes, "the bytes survive the encoding exactly");
            }
            other => panic!("expected a GitChunk, got {other:?}"),
        }
    }

    #[test]
    fn the_git_chunk_ceiling_leaves_room_for_base64_and_the_envelope() {
        let encoded = GIT_STREAM_CHUNK_MAX / 3 * 4 + 4;
        assert!(
            encoded + 4096 < MAX_FRAME,
            "a full chunk encodes to {encoded} bytes against a {MAX_FRAME}-byte frame limit"
        );
        const {
            assert!(
                GIT_STREAM_CHUNK_MAX > GIT_STREAM_CHUNK,
                "the ceiling is a backstop for an oversized line, not the batch target"
            )
        };
    }

    #[test]
    fn events_carry_a_zero_req_id() {
        let mut buf = Vec::new();
        ControlServerMsg::Event(ControlEvent::WatchOverflow { id: 1 })
            .encode(&mut buf)
            .unwrap();
        assert_eq!(buf[4], 63);
        assert_eq!(&buf[5..13], &0u64.to_le_bytes());
    }

    #[test]
    fn short_payload_is_invalid_data() {
        for len in 0..CONTROL_HEADER {
            let payload = vec![0u8; len];
            for k in [kind::REQUEST, kind::REQUEST_BLOB, kind::CANCEL] {
                let e = ControlClientMsg::from_frame(k, payload.clone()).unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::InvalidData, "kind {k}, len {len}");
            }
            for k in [kind::RESPONSE, kind::RESPONSE_BLOB, kind::EVENT] {
                let e = ControlServerMsg::from_frame(k, payload.clone()).unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::InvalidData, "kind {k}, len {len}");
            }
        }
    }

    #[test]
    fn oversized_json_len_is_invalid_data_not_a_panic() {
        for json_n in [13u32, 1_000_000, u32::MAX] {
            let mut payload = Vec::new();
            payload.extend_from_slice(&1u64.to_le_bytes());
            payload.extend_from_slice(&json_n.to_le_bytes());
            payload.extend_from_slice(b"{}");
            let e = ControlClientMsg::from_frame(kind::REQUEST, payload.clone()).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "json_n {json_n}");
            let e = ControlServerMsg::from_frame(kind::RESPONSE_BLOB, payload).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "json_n {json_n}");
        }
    }

    #[test]
    fn trailing_bytes_on_a_non_blob_kind_are_invalid_data() {
        let json = serde_json::to_vec(&ControlRequest::Ping).unwrap();
        let mut payload = encode_body(1, &json, b"extra").unwrap();
        let e = ControlClientMsg::from_frame(kind::REQUEST, payload.clone()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);

        payload = encode_body(1, &[], b"extra").unwrap();
        let e = ControlClientMsg::from_frame(kind::CANCEL, payload).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);

        let json = serde_json::to_vec(&ControlEvent::WatchOverflow { id: 1 }).unwrap();
        let payload = encode_body(0, &json, b"extra").unwrap();
        let e = ControlServerMsg::from_frame(kind::EVENT, payload).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn event_with_a_nonzero_req_id_is_invalid_data() {
        let json = serde_json::to_vec(&ControlEvent::WatchOverflow { id: 1 }).unwrap();
        let payload = encode_body(9, &json, &[]).unwrap();
        let e = ControlServerMsg::from_frame(kind::EVENT, payload).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn zero_req_id_is_rejected_on_requests_and_responses() {
        let json = serde_json::to_vec(&ControlRequest::Ping).unwrap();
        let payload = encode_body(0, &json, &[]).unwrap();
        for k in [kind::REQUEST, kind::CANCEL] {
            let e = ControlClientMsg::from_frame(k, payload.clone()).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "kind {k}");
        }
        let e = ControlClientMsg::from_frame(kind::REQUEST_BLOB, payload).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);

        let json = serde_json::to_vec(&ControlReply::Ok(ReplyOk::Unit)).unwrap();
        let payload = encode_body(0, &json, &[]).unwrap();
        for k in [kind::RESPONSE, kind::RESPONSE_BLOB] {
            let e = ControlServerMsg::from_frame(k, payload.clone()).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "kind {k}");
        }

        let mut buf = Vec::new();
        assert!(
            ControlClientMsg::Request {
                req_id: 0,
                req: ControlRequest::Ping
            }
            .encode(&mut buf)
            .is_err()
        );
    }

    #[test]
    fn unknown_kinds_are_invalid_data() {
        for k in [0u8, 1, 13, 40, 50, 59, 64, 69, 200, 255] {
            let e = ControlClientMsg::from_frame(k, vec![0; 32]).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "client kind {k}");
            let e = ControlServerMsg::from_frame(k, vec![0; 32]).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "server kind {k}");
        }
    }

    #[test]
    fn control_kinds_sit_in_their_own_range() {
        let ours = [
            kind::HELLO,
            kind::REQUEST,
            kind::REQUEST_BLOB,
            kind::CANCEL,
            kind::HELLO_OK,
            kind::RESPONSE,
            kind::RESPONSE_BLOB,
            kind::EVENT,
        ];
        for k in ours {
            assert!((60..=63).contains(&k), "control kind {k} left its range");
            assert_ne!(k, 13, "13 is retired and must never be reused");
        }
        let taken: Vec<u8> = (1..=24).chain(30..=36).chain([40, 50]).collect();
        for k in ours {
            assert!(
                !taken.contains(&k),
                "control kind {k} collides with protocol"
            );
        }
    }

    #[test]
    fn malformed_json_is_invalid_data() {
        let payload = encode_body(1, b"{not json", &[]).unwrap();
        let e = ControlClientMsg::from_frame(kind::REQUEST, payload).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);

        let e = ControlClientMsg::from_frame(kind::HELLO, b"nope".to_vec()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversize_blob_is_refused_at_encode() {
        let blob = vec![0u8; MAX_FRAME];
        let mut sink = Vec::new();
        let e = ControlClientMsg::RequestBlob {
            req_id: 1,
            req: ControlRequest::WriteFile {
                path: "/tmp/huge".into(),
            },
            blob,
        }
        .encode(&mut sink)
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversize_declared_length_is_refused_at_decode() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_FRAME + 1) as u32).to_le_bytes());
        buf.push(kind::REQUEST);
        let e = ControlClientMsg::read(&mut Cursor::new(&buf)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_frame_exactly_at_max_frame_survives() {
        let json = serde_json::to_vec(&ControlRequest::WriteFile {
            path: "/tmp/big".into(),
        })
        .unwrap();
        let blob = vec![0x5a; MAX_FRAME - CONTROL_HEADER - json.len()];
        let msg = ControlClientMsg::RequestBlob {
            req_id: 1,
            req: ControlRequest::WriteFile {
                path: "/tmp/big".into(),
            },
            blob,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        assert_eq!(
            u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize,
            MAX_FRAME
        );
        assert_eq!(ControlClientMsg::read(&mut Cursor::new(&buf)).unwrap(), msg);
    }

    #[test]
    fn error_kinds_round_trip_through_io() {
        use io::ErrorKind as K;
        let pairs = [
            (K::NotFound, WireErrorKind::NotFound),
            (K::PermissionDenied, WireErrorKind::PermissionDenied),
            (K::AlreadyExists, WireErrorKind::AlreadyExists),
            (K::InvalidInput, WireErrorKind::InvalidInput),
            (K::NotADirectory, WireErrorKind::NotADirectory),
            (K::IsADirectory, WireErrorKind::IsADirectory),
            (K::DirectoryNotEmpty, WireErrorKind::DirectoryNotEmpty),
            (K::FileTooLarge, WireErrorKind::FileTooLarge),
            (K::TimedOut, WireErrorKind::TimedOut),
            (K::ConnectionReset, WireErrorKind::ConnectionReset),
        ];
        for (io_kind, wire_kind) in pairs {
            let wire = WireError::from_io(&io::Error::new(io_kind, "boom"));
            assert_eq!(wire.kind, wire_kind, "io {io_kind:?} -> wire");
            assert_eq!(wire.into_io().kind(), io_kind, "wire {wire_kind:?} -> io");
        }
    }

    #[test]
    fn error_kinds_that_collapse_do_so_predictably() {
        use io::ErrorKind as K;
        for k in [K::BrokenPipe, K::UnexpectedEof, K::ConnectionReset] {
            assert_eq!(
                WireError::from_io(&io::Error::new(k, "x")).kind,
                WireErrorKind::ConnectionReset
            );
        }
        assert_eq!(
            WireError::from_io(&io::Error::new(K::InvalidFilename, "x")).kind,
            WireErrorKind::InvalidInput
        );
        assert_eq!(
            WireError::from_io(&io::Error::new(K::WouldBlock, "x")).kind,
            WireErrorKind::Other
        );
        assert_eq!(WireErrorKind::GitUnavailable.to_io_kind(), K::NotFound);
    }

    #[test]
    fn wire_error_message_survives_into_io() {
        let e = WireError::new(WireErrorKind::NotFound, "/home/me/gone: no such file").into_io();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("/home/me/gone"));
    }

    #[test]
    fn a_nonzero_git_exit_is_ok_not_err() {
        let reply = ControlReply::Ok(ReplyOk::Output(Output {
            status: Some(128),
            stdout: Vec::new(),
            stderr: b"not a git repository".to_vec(),
        }));
        let mut buf = Vec::new();
        ControlServerMsg::Response { req_id: 1, reply }
            .encode(&mut buf)
            .unwrap();
        match ControlServerMsg::read(&mut Cursor::new(&buf)).unwrap() {
            ControlServerMsg::Response { reply, .. } => {
                let ok = reply
                    .into_result()
                    .expect("a non-zero exit is not an error");
                match ok {
                    ReplyOk::Output(o) => {
                        assert_eq!(o.status, Some(128));
                        assert!(!o.success());
                        assert_eq!(o.stderr_trimmed(), "not a git repository");
                    }
                    other => panic!("expected Output, got {other:?}"),
                }
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[test]
    fn output_bytes_ride_as_base64_not_a_number_array() {
        let out = Output {
            status: Some(0),
            stdout: vec![0u8; 3000],
            stderr: Vec::new(),
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(
            json.contains("\"stdout\":\"AAAA"),
            "stdout should be a base64 string, got: {}",
            &json[..60.min(json.len())]
        );
        assert!(!json.contains("[0,0,0"), "must not be a number array");
        assert!(json.len() < 4200, "base64 payload is {} bytes", json.len());
        assert_eq!(serde_json::from_str::<Output>(&json).unwrap(), out);
    }

    #[test]
    fn deadlines_match_the_contract_table() {
        use ControlRequest as R;
        let s = Duration::from_secs;
        let cases: Vec<(R, Duration)> = vec![
            (R::Ping, s(5)),
            (
                R::ReadDir {
                    dir: "/".into(),
                    root: None,
                },
                s(5),
            ),
            (
                R::ReadDir {
                    dir: "/".into(),
                    root: Some("/".into()),
                },
                s(5),
            ),
            (R::Stat { path: "/".into() }, s(5)),
            (R::Exists { path: "/".into() }, s(5)),
            (R::Canonicalize { path: "/".into() }, s(5)),
            (R::RepoRoot { path: "/".into() }, s(5)),
            (R::WatchOpen { dirs: vec![] }, s(5)),
            (
                R::WatchSet {
                    id: 1,
                    dirs: vec![],
                },
                s(5),
            ),
            (R::WatchClose { id: 1 }, s(5)),
            (
                R::ReadFile {
                    path: "/".into(),
                    max_bytes: 1,
                },
                s(30),
            ),
            (R::WriteFile { path: "/".into() }, s(30)),
            (R::CreateFileNew { path: "/".into() }, s(10)),
            (
                R::CreateDir {
                    path: "/".into(),
                    recursive: false,
                },
                s(10),
            ),
            (
                R::Rename {
                    from: "/".into(),
                    to: "/".into(),
                },
                s(10),
            ),
            (
                R::Remove {
                    path: "/".into(),
                    recursive: false,
                },
                s(10),
            ),
            (
                R::Git {
                    cwd: "/".into(),
                    args: vec![],
                },
                s(20),
            ),
            (
                R::Search {
                    roots: vec![],
                    query: String::new(),
                    limit: 1,
                    max_dirs: 1,
                    show_hidden: true,
                },
                s(20),
            ),
            (R::GuiOpen { path: None }, s(5)),
            (R::AgentStates, s(5)),
            (R::Routes, s(5)),
            (R::Status, s(5)),
        ];
        assert_eq!(
            cases.len(),
            every_request().len(),
            "every request variant needs a deadline case"
        );
        for (req, want) in cases {
            assert_eq!(req.deadline(), want, "deadline for {req:?}");
        }
    }

    #[test]
    fn blob_shape_is_known_per_request() {
        for req in every_request() {
            let takes = matches!(req, ControlRequest::WriteFile { .. });
            let returns = matches!(req, ControlRequest::ReadFile { .. });
            assert_eq!(req.takes_blob(), takes, "takes_blob for {req:?}");
            assert_eq!(req.returns_blob(), returns, "returns_blob for {req:?}");
        }
    }

    #[test]
    fn hello_ok_round_trips_with_features() {
        let mut buf = Vec::new();
        ControlServerMsg::HelloOk(hello_ok())
            .encode(&mut buf)
            .unwrap();
        match ControlServerMsg::read(&mut Cursor::new(&buf)).unwrap() {
            ControlServerMsg::HelloOk(ok) => {
                assert_eq!(ok.separator, '/');
                assert_eq!(ok.home, "/home/me");
                assert!(ok.has_feature(feature::CONTROL));
                assert!(ok.has_feature(feature::HOST_RPC));
                assert!(!ok.has_feature("workspace-store"));
            }
            other => panic!("expected HelloOk, got {other:?}"),
        }
    }

    #[test]
    fn hello_ok_without_features_still_decodes() {
        let json = br#"{"control_version":1,"protocol_version":3,"build":"old",
                        "separator":"/","home":"/root"}"#;
        let ok: ControlHelloOk = serde_json::from_slice(json).unwrap();
        assert!(ok.features.is_empty());
        assert!(!ok.has_feature(feature::CONTROL));
    }

    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    fn client_with_peer<F>(events: EventSink, serve: F) -> ControlClient
    where
        F: FnOnce(TcpStream) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Hello(h) => assert_eq!(h.control_version, CONTROL_VERSION),
                other => panic!("expected Hello, got {other:?}"),
            }
            ControlServerMsg::HelloOk(hello_ok())
                .encode(&mut sock)
                .unwrap();
            sock.flush().unwrap();
            serve(sock);
        });
        let sock = TcpStream::connect(addr).unwrap();
        ControlClient::over_tcp(sock, &hello(), events).unwrap()
    }

    fn no_events() -> EventSink {
        Box::new(|_| {})
    }

    fn reply_to<W: Write>(w: &mut W, req_id: u64, reply: ReplyOk) {
        ControlServerMsg::Response {
            req_id,
            reply: ControlReply::Ok(reply),
        }
        .encode(w)
        .unwrap();
        w.flush().unwrap();
    }

    #[test]
    fn a_request_and_its_reply_cross_a_real_duplex_stream() {
        let client = client_with_peer(no_events(), |mut sock| {
            for _ in 0..3 {
                match ControlClientMsg::read(&mut sock) {
                    Ok(ControlClientMsg::Request { req_id, req }) => {
                        let reply = match req {
                            ControlRequest::Ping => ReplyOk::Pong,
                            ControlRequest::Stat { .. } => ReplyOk::Meta(meta()),
                            ControlRequest::Exists { .. } => ReplyOk::Bool(true),
                            other => panic!("unexpected request {other:?}"),
                        };
                        reply_to(&mut sock, req_id, reply);
                    }
                    other => panic!("unexpected message {other:?}"),
                }
            }
        });

        assert_eq!(client.hello().separator, '/');
        assert_eq!(client.hello().home, "/home/me");
        assert!(client.is_connected());

        assert_eq!(client.call(ControlRequest::Ping).unwrap(), ReplyOk::Pong);
        assert_eq!(
            client
                .call(ControlRequest::Stat {
                    path: "/etc/hosts".into()
                })
                .unwrap(),
            ReplyOk::Meta(meta())
        );
        assert_eq!(
            client
                .call(ControlRequest::Exists {
                    path: "/etc".into()
                })
                .unwrap(),
            ReplyOk::Bool(true)
        );
    }

    #[test]
    fn a_slow_request_does_not_block_a_fast_one_behind_it() {
        let client = Arc::new(client_with_peer(no_events(), |mut sock| {
            let mut parked_git = None;
            loop {
                match ControlClientMsg::read(&mut sock).unwrap() {
                    ControlClientMsg::Request { req_id, req } => match req {
                        ControlRequest::Git { .. } => parked_git = Some(req_id),
                        ControlRequest::ReadDir { .. } => {
                            reply_to(&mut sock, req_id, ReplyOk::Entries(vec![]));
                            thread::sleep(Duration::from_millis(250));
                            reply_to(
                                &mut sock,
                                parked_git.expect("git arrived first"),
                                ReplyOk::Output(Output {
                                    status: Some(0),
                                    stdout: b"clean".to_vec(),
                                    stderr: Vec::new(),
                                }),
                            );
                            return;
                        }
                        other => panic!("unexpected request {other:?}"),
                    },
                    other => panic!("unexpected message {other:?}"),
                }
            }
        }));

        let (done_tx, done_rx) = mpsc::channel::<&'static str>();

        let slow_client = Arc::clone(&client);
        let slow_done = done_tx.clone();
        let slow = thread::spawn(move || {
            let out = slow_client
                .call(ControlRequest::Git {
                    cwd: "/repo".into(),
                    args: vec!["status".into()],
                })
                .unwrap();
            slow_done.send("git").unwrap();
            out
        });

        thread::sleep(Duration::from_millis(100));
        let entries = client
            .call(ControlRequest::ReadDir {
                dir: "/repo".into(),
                root: None,
            })
            .unwrap();
        done_tx.send("read_dir").unwrap();

        assert_eq!(entries, ReplyOk::Entries(vec![]));
        assert_eq!(
            done_rx.recv().unwrap(),
            "read_dir",
            "the fast request must finish first even though it was issued second"
        );
        assert_eq!(done_rx.recv().unwrap(), "git");
        assert!(matches!(
            slow.join().unwrap(),
            ReplyOk::Output(o) if o.stdout_trimmed() == "clean"
        ));
    }

    #[test]
    fn concurrent_callers_each_get_their_own_reply() {
        const N: u64 = 16;
        let client = Arc::new(client_with_peer(no_events(), |mut sock| {
            let mut seen = Vec::new();
            while (seen.len() as u64) < N {
                match ControlClientMsg::read(&mut sock).unwrap() {
                    ControlClientMsg::Request { req_id, req } => seen.push((req_id, req)),
                    other => panic!("unexpected message {other:?}"),
                }
            }
            for (req_id, req) in seen.into_iter().rev() {
                let path = match req {
                    ControlRequest::Canonicalize { path } => path,
                    other => panic!("unexpected request {other:?}"),
                };
                reply_to(&mut sock, req_id, ReplyOk::Path(path));
            }
        }));

        let handles: Vec<_> = (0..N)
            .map(|i| {
                let c = Arc::clone(&client);
                thread::spawn(move || {
                    let want = format!("/p/{i}");
                    let got = c
                        .call(ControlRequest::Canonicalize { path: want.clone() })
                        .unwrap();
                    assert_eq!(
                        got,
                        ReplyOk::Path(want),
                        "caller {i} received another caller's reply"
                    );
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn a_timeout_cancels_that_request_and_leaves_the_connection_usable() {
        let (saw_cancel_tx, saw_cancel_rx) = mpsc::channel();
        let client = client_with_peer(no_events(), move |mut sock| {
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Request { .. } => {}
                other => panic!("unexpected message {other:?}"),
            }
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Cancel { req_id } => saw_cancel_tx.send(req_id).unwrap(),
                other => panic!("expected Cancel, got {other:?}"),
            }
            reply_to(&mut sock, 1, ReplyOk::Path("/late".into()));
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Request { req_id, .. } => {
                    reply_to(&mut sock, req_id, ReplyOk::Pong)
                }
                other => panic!("unexpected message {other:?}"),
            }
        });

        let e = client
            .call_with_deadline(
                ControlRequest::Canonicalize {
                    path: "/slow".into(),
                },
                &[],
                Duration::from_millis(150),
            )
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert_eq!(saw_cancel_rx.recv().unwrap(), 1, "cancel names the request");

        assert!(client.is_connected(), "a timeout must not kill the link");
        assert_eq!(
            client.call(ControlRequest::Ping).unwrap(),
            ReplyOk::Pong,
            "the late reply to the abandoned request must not have been \
             mistaken for this one's"
        );
    }

    #[test]
    fn losing_the_link_fails_in_flight_requests_at_once() {
        let client = Arc::new(client_with_peer(no_events(), |mut sock| {
            let _ = ControlClientMsg::read(&mut sock);
            drop(sock);
        }));

        let started = Instant::now();
        let e = client
            .call(ControlRequest::ReadFile {
                path: "/big".into(),
                max_bytes: u64::MAX,
            })
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionReset);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "should fail on the hangup, not wait out the 30s ReadFile deadline"
        );
        assert!(!client.is_connected());

        let e = client.call(ControlRequest::Ping).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionReset);
    }

    #[test]
    fn blobs_ride_in_both_directions() {
        let content: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
        let echoed = content.clone();
        let client = client_with_peer(no_events(), move |mut sock| {
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::RequestBlob { req_id, req, blob } => {
                    assert_eq!(
                        req,
                        ControlRequest::WriteFile {
                            path: "/tmp/f".into()
                        }
                    );
                    assert_eq!(blob, echoed);
                    reply_to(&mut sock, req_id, ReplyOk::Meta(meta()));
                }
                other => panic!("expected RequestBlob, got {other:?}"),
            }
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Request { req_id, .. } => {
                    ControlServerMsg::ResponseBlob {
                        req_id,
                        reply: ControlReply::Ok(ReplyOk::FileMeta { meta: meta() }),
                        blob: echoed.clone(),
                    }
                    .encode(&mut sock)
                    .unwrap();
                    sock.flush().unwrap();
                }
                other => panic!("expected Request, got {other:?}"),
            }
        });

        let wrote = client
            .call_with_blob(
                ControlRequest::WriteFile {
                    path: "/tmp/f".into(),
                },
                &content,
            )
            .unwrap();
        assert_eq!(wrote, ReplyOk::Meta(meta()));

        let read = client
            .call_full(
                ControlRequest::ReadFile {
                    path: "/tmp/f".into(),
                    max_bytes: u64::MAX,
                },
                &[],
            )
            .unwrap();
        assert_eq!(read.reply, ReplyOk::FileMeta { meta: meta() });
        assert_eq!(read.blob, content, "the blob is the file's content");
    }

    #[test]
    fn an_error_reply_becomes_a_matching_io_error() {
        let client = client_with_peer(no_events(), |mut sock| {
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Request { req_id, .. } => {
                    ControlServerMsg::Response {
                        req_id,
                        reply: ControlReply::Err(WireError::new(
                            WireErrorKind::FileTooLarge,
                            "/big is 900 MB, over the 10 MB limit",
                        )),
                    }
                    .encode(&mut sock)
                    .unwrap();
                    sock.flush().unwrap();
                }
                other => panic!("unexpected message {other:?}"),
            }
        });

        let e = client
            .call(ControlRequest::ReadFile {
                path: "/big".into(),
                max_bytes: 10 * 1024 * 1024,
            })
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::FileTooLarge);
        assert!(e.to_string().contains("900 MB"));
    }

    #[test]
    fn events_reach_the_sink_interleaved_with_replies() {
        let (tx, rx) = mpsc::channel();
        let sink: EventSink = Box::new(move |e| tx.send(e).unwrap());
        let client = client_with_peer(sink, |mut sock| {
            ControlServerMsg::Event(ControlEvent::Watch {
                id: 1,
                paths: vec!["/a".into()],
            })
            .encode(&mut sock)
            .unwrap();
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Request { req_id, .. } => {
                    ControlServerMsg::Event(ControlEvent::WatchOverflow { id: 1 })
                        .encode(&mut sock)
                        .unwrap();
                    reply_to(&mut sock, req_id, ReplyOk::Pong);
                }
                other => panic!("unexpected message {other:?}"),
            }
            ControlServerMsg::Event(ControlEvent::Preempted {
                workspace: "w1".into(),
                by: "other".into(),
            })
            .encode(&mut sock)
            .unwrap();
            sock.flush().unwrap();
        });

        assert_eq!(client.call(ControlRequest::Ping).unwrap(), ReplyOk::Pong);
        let mut got = Vec::new();
        for _ in 0..3 {
            got.push(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        }
        assert_eq!(
            got,
            vec![
                ControlEvent::Watch {
                    id: 1,
                    paths: vec!["/a".into()]
                },
                ControlEvent::WatchOverflow { id: 1 },
                ControlEvent::Preempted {
                    workspace: "w1".into(),
                    by: "other".into()
                },
            ]
        );
    }

    #[test]
    fn request_ids_start_at_one_and_increase() {
        let (tx, rx) = mpsc::channel();
        let client = client_with_peer(no_events(), move |mut sock| {
            for _ in 0..4 {
                match ControlClientMsg::read(&mut sock).unwrap() {
                    ControlClientMsg::Request { req_id, .. } => {
                        tx.send(req_id).unwrap();
                        reply_to(&mut sock, req_id, ReplyOk::Pong);
                    }
                    other => panic!("unexpected message {other:?}"),
                }
            }
        });
        for _ in 0..4 {
            client.call(ControlRequest::Ping).unwrap();
        }
        let ids: Vec<u64> = (0..4).map(|_| rx.recv().unwrap()).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }

    #[test]
    fn a_control_version_mismatch_names_both_versions() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = ControlClientMsg::read(&mut sock);
            let mut ok = hello_ok();
            ok.control_version = CONTROL_VERSION + 7;
            ok.build = "from-the-future".into();
            ControlServerMsg::HelloOk(ok).encode(&mut sock).unwrap();
            sock.flush().unwrap();
        });

        let sock = TcpStream::connect(addr).unwrap();
        let e = ControlClient::over_tcp(sock, &hello(), no_events()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        let msg = e.to_string();
        assert!(msg.contains("from-the-future"), "names the peer: {msg}");
        assert!(
            msg.contains(&format!("v{}", CONTROL_VERSION + 7)),
            "names their version: {msg}"
        );
        assert!(
            msg.contains(&format!("v{CONTROL_VERSION}")),
            "names ours: {msg}"
        );
        assert!(
            is_dialect_refusal(&msg),
            "and the GUI can tell this apart from an unreachable host, which is what \
             decides whether it may offer to replace the far end's binary: {msg}"
        );
    }

    #[test]
    fn other_connect_failures_are_not_dialect_refusals() {
        for other in [
            "Connection refused (os error 61)",
            "ssh: handshake failed: no matching key exchange method",
            "the remote tty7-server did not start: exit status 127",
            "could not resolve the remote home directory: Permission denied",
            "control peer answered the handshake with Bye instead of HELLO_OK",
        ] {
            assert!(!is_dialect_refusal(other), "{other}");
        }
        assert!(is_dialect_refusal(&dialect_refusal("26.7.6", 2, 3)));
    }

    #[test]
    fn a_dialect_refusal_parses_back_into_its_parts() {
        let wrapped = format!("java answered: {}", dialect_refusal("26.7.7-nightly", 4, 5));
        assert_eq!(
            parse_dialect_refusal(&wrapped),
            Some(DialectRefusal {
                peer_build: "26.7.7-nightly".into(),
                peer: 4,
                ours: 5,
            }),
            "the GUI reads the build and both versions back out to say which side is behind"
        );
        for other in [
            "Connection refused (os error 61)",
            "control peer answered the handshake with Bye instead of HELLO_OK",
        ] {
            assert_eq!(parse_dialect_refusal(other), None, "{other}");
        }
    }

    #[test]
    fn a_handshake_answered_with_the_wrong_frame_is_invalid_data() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = ControlClientMsg::read(&mut sock);
            ControlServerMsg::Event(ControlEvent::WatchOverflow { id: 1 })
                .encode(&mut sock)
                .unwrap();
            sock.flush().unwrap();
        });
        let sock = TcpStream::connect(addr).unwrap();
        let e = ControlClient::over_tcp(sock, &hello(), no_events()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn closing_an_idle_client_returns_promptly() {
        let client = client_with_peer(no_events(), |mut sock| {
            let _ = ControlClientMsg::read(&mut sock);
            thread::sleep(Duration::from_secs(30));
        });
        assert_eq!(client.call(ControlRequest::Ping).unwrap_err().kind(), {
            io::ErrorKind::TimedOut
        });

        let started = Instant::now();
        client.close();
        assert!(
            started.elapsed() < CLOSE_GRACE * 2,
            "close() took {:?}; it must not wait on a peer that will never speak",
            started.elapsed()
        );
    }

    #[test]
    fn dropping_an_idle_client_returns_promptly() {
        let client = client_with_peer(no_events(), |mut sock| {
            let _ = ControlClientMsg::read(&mut sock);
            thread::sleep(Duration::from_secs(30));
        });
        let started = Instant::now();
        drop(client);
        assert!(
            started.elapsed() < CLOSE_GRACE * 2,
            "drop took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn close_reaps_the_reader_when_the_link_can_be_shut_down() {
        let client = client_with_peer(no_events(), |mut sock| {
            let _ = ControlClientMsg::read(&mut sock);
            thread::sleep(Duration::from_secs(30));
        });
        let started = Instant::now();
        client.close();
        assert!(
            started.elapsed() < CLOSE_GRACE,
            "reader should have been reaped, not waited out: {:?}",
            started.elapsed()
        );
        assert!(!client.is_connected());
    }

    #[test]
    fn a_malformed_frame_tears_the_connection_down() {
        let client = client_with_peer(no_events(), |mut sock| {
            let _ = ControlClientMsg::read(&mut sock);
            write_frame(&mut sock, 64, &[0u8; 16]).unwrap();
            sock.flush().unwrap();
            thread::sleep(Duration::from_secs(2));
        });
        let e = client.call(ControlRequest::Ping).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionReset);
        assert!(!client.is_connected());
    }
}

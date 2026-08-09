use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::daemon::install::{
    InstallConfirm, InstallDecision, InstallPhase, InstallProgress, InstallRequest,
    MismatchedRemoteDaemon,
};
use crate::daemon::protocol::{self, AuthPromptKind, AuthResponse, DaemonMsg, NativeSshSpec};
use crate::daemon::remote_link::RemoteLink;
use crate::daemon::ssh::{ConnectionKey, PromptBroker, SshConnection, SshManager};
use crate::daemon::transport::Stream;

pub const ROUTE_KIND: u8 = 51;

pub const ROUTE_PROMPT_KIND: u8 = 52;

pub const ROUTE_REPLY_KIND: u8 = 53;

const REPLY_TIMEOUT: Duration = Duration::from_secs(240);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteChannel {
    #[default]
    Control,
    Pane,
}

impl RouteChannel {
    pub(crate) fn bridge_command(self, base: &str) -> String {
        match self {
            RouteChannel::Control => base.to_string(),
            RouteChannel::Pane => format!("{base} --pane"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteTarget {
    Ssh(Box<NativeSshSpec>),
    Wsl { distro: String },
    LocalStdio { program: String, args: Vec<String> },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteAction {
    #[default]
    Forward,
    RestartServer,
    ReplaceServer,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouteHeader {
    pub target: RouteTarget,
    #[serde(default)]
    pub server_command: Option<String>,
    #[serde(default)]
    pub channel: RouteChannel,
    #[serde(default)]
    pub action: RouteAction,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RouteAck {
    pub ok: bool,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub action: Option<RouteAction>,
    #[serde(default)]
    pub error: Option<String>,
}

impl RouteHeader {
    pub fn ssh(spec: NativeSshSpec) -> RouteHeader {
        RouteHeader {
            target: RouteTarget::Ssh(Box::new(spec)),
            server_command: None,
            channel: RouteChannel::Control,
            action: RouteAction::Forward,
        }
    }

    pub fn for_pane(mut self) -> RouteHeader {
        self.channel = RouteChannel::Pane;
        self
    }

    pub fn restart_server(mut self) -> RouteHeader {
        self.action = RouteAction::RestartServer;
        self
    }

    pub fn replace_server(mut self) -> RouteHeader {
        self.action = RouteAction::ReplaceServer;
        self
    }

    pub fn wsl(distro: impl Into<String>) -> RouteHeader {
        RouteHeader {
            target: RouteTarget::Wsl {
                distro: distro.into(),
            },
            server_command: None,
            channel: RouteChannel::Control,
            action: RouteAction::Forward,
        }
    }

    pub fn local_stdio(program: impl Into<String>, args: &[&str]) -> RouteHeader {
        RouteHeader {
            target: RouteTarget::LocalStdio {
                program: program.into(),
                args: args.iter().map(|a| (*a).to_string()).collect(),
            },
            server_command: None,
            channel: RouteChannel::Control,
            action: RouteAction::Forward,
        }
    }

    pub fn write<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let payload =
            serde_json::to_vec(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        protocol::write_frame(w, ROUTE_KIND, &payload)?;
        w.flush()
    }

    pub fn decode(payload: &[u8]) -> io::Result<RouteHeader> {
        serde_json::from_slice(payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn describe(&self) -> String {
        match &self.target {
            RouteTarget::Ssh(spec) => format!("ssh {}@{}:{}", spec.user, spec.host, spec.port),
            RouteTarget::Wsl { distro } => format!("wsl {distro}"),
            RouteTarget::LocalStdio { program, .. } => format!("local {program}"),
        }
    }
}

impl RouteTarget {
    pub fn origin_key(&self) -> String {
        match self {
            RouteTarget::Ssh(spec) => ConnectionKey::from_spec(spec).as_str().to_string(),
            RouteTarget::Wsl { distro } => format!("wsl:{distro}"),
            RouteTarget::LocalStdio { program, .. } => format!("local-stdio:{program}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRequestWire {
    pub host: String,
    pub version: String,
    pub asset: String,
    pub source_url: String,
    pub remote_path: String,
    pub size_bytes: u64,
    pub sha256: String,
}

impl InstallRequestWire {
    fn from_request(request: &InstallRequest) -> InstallRequestWire {
        InstallRequestWire {
            host: request.host.clone(),
            version: request.version.clone(),
            asset: request.asset.to_string(),
            source_url: request.source_url.clone(),
            remote_path: request.remote_path.clone(),
            size_bytes: request.size_bytes,
            sha256: request.sha256.clone(),
        }
    }

    pub fn into_request(self) -> InstallRequest {
        InstallRequest {
            host: self.host,
            version: self.version,
            asset: crate::daemon::install::asset::interned(&self.asset),
            source_url: self.source_url,
            remote_path: self.remote_path,
            size_bytes: self.size_bytes,
            sha256: self.sha256,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutePrompt {
    Auth {
        request_id: u64,
        prompt: AuthPromptKind,
    },
    Install {
        request_id: u64,
        request: Box<InstallRequestWire>,
    },
    Mismatch {
        daemons: Vec<MismatchedRemoteDaemon>,
    },
    InstallProgress {
        host: String,
        phase: InstallPhase,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteReply {
    Auth {
        request_id: u64,
        response: AuthResponse,
    },
    Install {
        request_id: u64,
        approve: bool,
    },
}

impl RoutePrompt {
    #[cfg(test)]
    fn write<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let payload =
            serde_json::to_vec(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        protocol::write_frame(w, ROUTE_PROMPT_KIND, &payload)?;
        w.flush()
    }

    fn decode(payload: &[u8]) -> io::Result<RoutePrompt> {
        serde_json::from_slice(payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

impl RouteReply {
    fn write<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let payload =
            serde_json::to_vec(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        protocol::write_frame(w, ROUTE_REPLY_KIND, &payload)?;
        w.flush()
    }

    fn decode(payload: &[u8]) -> io::Result<RouteReply> {
        serde_json::from_slice(payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

pub trait RouteAuthResponder: Send + Sync {
    fn respond(&self, machine: &RouteTarget, prompt: &AuthPromptKind) -> AuthResponse;
}

pub struct CancelAuth;

impl RouteAuthResponder for CancelAuth {
    fn respond(&self, _machine: &RouteTarget, _prompt: &AuthPromptKind) -> AuthResponse {
        AuthResponse::Cancelled
    }
}

static AUTH_RESPONDER: OnceLock<Mutex<Arc<dyn RouteAuthResponder>>> = OnceLock::new();

fn auth_responder_slot() -> &'static Mutex<Arc<dyn RouteAuthResponder>> {
    AUTH_RESPONDER.get_or_init(|| Mutex::new(Arc::new(CancelAuth)))
}

pub fn set_route_auth_responder(responder: Arc<dyn RouteAuthResponder>) {
    if let Ok(mut slot) = auth_responder_slot().lock() {
        *slot = responder;
    }
}

pub fn route_auth_responder() -> Arc<dyn RouteAuthResponder> {
    auth_responder_slot()
        .lock()
        .map(|slot| slot.clone())
        .unwrap_or_else(|_| Arc::new(CancelAuth))
}

pub fn negotiate<S>(stream: &mut S, header: &RouteHeader) -> io::Result<RouteAck>
where
    for<'a> &'a mut S: Read + Write,
{
    header.write(&mut &mut *stream)?;
    loop {
        let (kind, payload) = protocol::read_frame(&mut &mut *stream)?;
        match kind {
            ROUTE_KIND => return RouteAck::from_payload(&payload),
            ROUTE_PROMPT_KIND => {
                if let Some(reply) = answer(&header.target, RoutePrompt::decode(&payload)?) {
                    reply.write(&mut &mut *stream)?;
                }
            }
            other => {
                if let Ok(DaemonMsg::Error(e)) = DaemonMsg::from_frame(other, payload) {
                    return Err(io::Error::other(e));
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected a route ack, got kind {other}"),
                ));
            }
        }
    }
}

fn answer(machine: &RouteTarget, prompt: RoutePrompt) -> Option<RouteReply> {
    match prompt {
        RoutePrompt::Auth { request_id, prompt } => {
            let response = route_auth_responder().respond(machine, &prompt);
            Some(RouteReply::Auth {
                request_id,
                response,
            })
        }
        RoutePrompt::Install {
            request_id,
            request,
        } => {
            let decision =
                crate::daemon::install::install_confirm().confirm(&request.into_request());
            Some(RouteReply::Install {
                request_id,
                approve: decision == InstallDecision::Approve,
            })
        }
        RoutePrompt::Mismatch { daemons } => {
            crate::daemon::install::record_remote_mismatches(daemons);
            None
        }
        RoutePrompt::InstallProgress { host, phase } => {
            crate::daemon::install::install_progress().report(&host, phase);
            None
        }
    }
}

impl RouteAck {
    fn ok(link: &RemoteLink) -> RouteAck {
        RouteAck {
            ok: true,
            link: Some(link.kind_label().to_string()),
            action: Some(RouteAction::Forward),
            error: None,
        }
    }

    fn acted(action: RouteAction) -> RouteAck {
        RouteAck {
            ok: true,
            link: None,
            action: Some(action),
            error: None,
        }
    }

    fn failed(error: String) -> RouteAck {
        RouteAck {
            ok: false,
            link: None,
            action: None,
            error: Some(error),
        }
    }

    pub fn performed(&self, action: RouteAction) -> bool {
        self.ok && self.action == Some(action)
    }

    pub fn read<R: io::Read>(r: &mut R) -> io::Result<RouteAck> {
        let (kind, payload) = protocol::read_frame(r)?;
        if kind != ROUTE_KIND {
            if let Ok(DaemonMsg::Error(e)) = DaemonMsg::from_frame(kind, payload) {
                return Err(io::Error::other(e));
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("expected a route ack, got kind {kind}"),
            ));
        }
        RouteAck::from_payload(&payload)
    }

    fn from_payload(payload: &[u8]) -> io::Result<RouteAck> {
        let ack: RouteAck = serde_json::from_slice(payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if !ack.ok {
            return Err(io::Error::other(
                ack.error.unwrap_or_else(|| "route refused".into()),
            ));
        }
        Ok(ack)
    }

    #[cfg(test)]
    fn write<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let payload =
            serde_json::to_vec(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        protocol::write_frame(w, ROUTE_KIND, &payload)?;
        w.flush()
    }
}

pub struct RouteSetup {
    pub broker: Arc<PromptBroker>,
    pub confirm: Arc<dyn InstallConfirm>,
    pub progress: Arc<dyn InstallProgress>,
    pub mismatches: Arc<Mutex<Vec<MismatchedRemoteDaemon>>>,
    pub channel: RouteChannel,
}

impl RouteSetup {
    pub fn unattended(channel: RouteChannel) -> RouteSetup {
        RouteSetup {
            broker: PromptBroker::new(Box::new(|_| false)),
            confirm: Arc::new(crate::daemon::install::DenyInstall),
            progress: Arc::new(crate::daemon::install::SilentProgress),
            mismatches: Arc::new(Mutex::new(Vec::new())),
            channel,
        }
    }

    pub async fn blocking<T, F>(&self, f: F) -> io::Result<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let confirm = self.confirm.clone();
        let progress = self.progress.clone();
        let sink = self.mismatches.clone();
        tokio::task::spawn_blocking(move || {
            crate::daemon::install::with_install_confirm(confirm, || {
                crate::daemon::install::with_install_progress(progress, || {
                    crate::daemon::install::with_mismatch_sink(sink, f)
                })
            })
        })
        .await
        .map_err(io::Error::other)
    }
}

struct Relay {
    out: tokio::sync::mpsc::UnboundedSender<(u8, Vec<u8>)>,
    pending: Mutex<HashMap<u64, std::sync::mpsc::SyncSender<bool>>>,
    next_id: AtomicU64,
}

impl Relay {
    fn fulfil(&self, request_id: u64, approve: bool) {
        if let Ok(mut pending) = self.pending.lock()
            && let Some(tx) = pending.remove(&request_id)
        {
            let _ = tx.send(approve);
        }
    }

    fn forget(&self, request_id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&request_id);
        }
    }
}

impl InstallConfirm for Relay {
    fn confirm(&self, request: &InstallRequest) -> InstallDecision {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(request_id, tx);
        } else {
            return InstallDecision::Decline;
        }

        let prompt = RoutePrompt::Install {
            request_id,
            request: Box::new(InstallRequestWire::from_request(request)),
        };
        let sent = serde_json::to_vec(&prompt)
            .ok()
            .is_some_and(|payload| self.out.send((ROUTE_PROMPT_KIND, payload)).is_ok());
        if !sent {
            self.forget(request_id);
            return InstallDecision::Decline;
        }

        match rx.recv_timeout(REPLY_TIMEOUT) {
            Ok(true) => InstallDecision::Approve,
            _ => {
                self.forget(request_id);
                InstallDecision::Decline
            }
        }
    }
}

impl InstallProgress for Relay {
    fn report(&self, host: &str, phase: InstallPhase) {
        let prompt = RoutePrompt::InstallProgress {
            host: host.to_string(),
            phase,
        };
        if let Ok(payload) = serde_json::to_vec(&prompt) {
            let _ = self.out.send((ROUTE_PROMPT_KIND, payload));
        }
    }
}

pub struct RemoteRouter;

impl RemoteRouter {
    pub fn route(local: Stream, header: &RouteHeader) -> io::Result<()> {
        SshManager::global().handle().block_on(drive(local, header))
    }
}

async fn drive(local: Stream, header: &RouteHeader) -> io::Result<()> {
    let mut local = into_async(local)?;

    let (out, mut outbox) = tokio::sync::mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    let emitter = out.clone();
    let relay = Arc::new(Relay {
        out,
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
    });
    let setup = RouteSetup {
        broker: PromptBroker::new(Box::new(move |msg| match msg {
            DaemonMsg::AuthPrompt { request_id, prompt } => {
                serde_json::to_vec(&RoutePrompt::Auth { request_id, prompt })
                    .is_ok_and(|payload| emitter.send((ROUTE_PROMPT_KIND, payload)).is_ok())
            }
            _ => true,
        })),
        confirm: relay.clone(),
        progress: relay.clone(),
        mismatches: Arc::new(Mutex::new(Vec::new())),
        channel: header.channel,
    };

    let Some((mut link, conn, leftover)) = ({
        let (mut read_half, mut write_half) = local.split();
        let mut frames = FrameReader::default();
        let mut opening = std::pin::pin!(perform(header, &setup));

        let opened = loop {
            tokio::select! {
                biased;
                result = &mut opening => break result,
                Some((kind, payload)) = outbox.recv() => {
                    write_frame(&mut write_half, kind, &payload).await?;
                }
                frame = frames.next(&mut read_half) => {
                    let (kind, payload) = frame?;
                    deliver(kind, &payload, &setup, &relay);
                }
            }
        };

        let found =
            std::mem::take(&mut *setup.mismatches.lock().unwrap_or_else(|e| e.into_inner()));
        if !found.is_empty()
            && let Ok(payload) = serde_json::to_vec(&RoutePrompt::Mismatch { daemons: found })
        {
            write_frame(&mut write_half, ROUTE_PROMPT_KIND, &payload).await?;
        }

        match opened {
            Ok(Performed::Linked(link, conn)) => {
                log::info!(
                    "routing a connection to {} over {}",
                    header.describe(),
                    link.kind_label()
                );
                let payload = ack_payload(&RouteAck::ok(&link))?;
                write_frame(&mut write_half, ROUTE_KIND, &payload).await?;
                Some((link, conn, frames.into_buffer()))
            }
            Ok(Performed::Acted(action)) => {
                log::info!("performed {action:?} on {}", header.describe());
                let payload = ack_payload(&RouteAck::acted(action))?;
                write_frame(&mut write_half, ROUTE_KIND, &payload).await?;
                None
            }
            Err(e) => {
                let message = format!("{e}");
                log::warn!("route to {} failed: {message}", header.describe());
                let payload = ack_payload(&RouteAck::failed(message.clone()))?;
                write_frame(&mut write_half, ROUTE_KIND, &payload).await?;
                return Err(io::Error::other(message));
            }
        }
    }) else {
        return Ok(());
    };

    if !leftover.is_empty() {
        tokio::io::AsyncWriteExt::write_all(&mut *link, &leftover).await?;
    }
    let (to_remote, to_local) = tokio::io::copy_bidirectional(&mut local, &mut *link).await?;
    log::debug!("routed connection closed after {to_remote} up / {to_local} down bytes");
    drop(conn);
    Ok(())
}

fn ack_payload(ack: &RouteAck) -> io::Result<Vec<u8>> {
    serde_json::to_vec(ack).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

async fn write_frame<W>(w: &mut W, kind: u8, payload: &[u8]) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt as _;
    if payload.len() > protocol::MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame payload exceeds MAX_FRAME",
        ));
    }
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.push(kind);
    frame.extend_from_slice(payload);
    w.write_all(&frame).await?;
    w.flush().await
}

#[derive(Default)]
struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    async fn next<R>(&mut self, r: &mut R) -> io::Result<(u8, Vec<u8>)>
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        use tokio::io::AsyncReadExt as _;
        loop {
            if let Some(frame) = protocol::take_frame(&mut self.buf)? {
                return Ok(frame);
            }
            let mut chunk = [0u8; 4096];
            let n = r.read(&mut chunk).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the client closed while the route was being set up",
                ));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    fn into_buffer(self) -> Vec<u8> {
        self.buf
    }
}

fn deliver(kind: u8, payload: &[u8], setup: &RouteSetup, relay: &Relay) {
    if kind != ROUTE_REPLY_KIND {
        log::debug!("ignoring kind {kind} during route setup");
        return;
    }
    match RouteReply::decode(payload) {
        Ok(RouteReply::Auth {
            request_id,
            response,
        }) => setup.broker.deliver(request_id, response),
        Ok(RouteReply::Install {
            request_id,
            approve,
        }) => relay.fulfil(request_id, approve),
        Err(e) => log::debug!("undecodable route reply: {e}"),
    }
}

enum Performed {
    Linked(Box<RemoteLink>, Option<Arc<SshConnection>>),
    Acted(RouteAction),
}

async fn perform(header: &RouteHeader, setup: &RouteSetup) -> anyhow::Result<Performed> {
    match header.action {
        RouteAction::Forward => {
            let (link, conn) = open_link(header, setup).await?;
            Ok(Performed::Linked(Box::new(link), conn))
        }
        action @ (RouteAction::RestartServer | RouteAction::ReplaceServer) => {
            restart_server(header, setup, action).await?;
            Ok(Performed::Acted(action))
        }
    }
}

async fn restart_server(
    header: &RouteHeader,
    setup: &RouteSetup,
    action: RouteAction,
) -> anyhow::Result<()> {
    match (&header.target, action) {
        (RouteTarget::Ssh(spec), RouteAction::ReplaceServer) => {
            SshManager::global()
                .replace_remote_server(spec, setup)
                .await
        }
        (RouteTarget::Ssh(spec), _) => {
            SshManager::global()
                .restart_remote_server(spec, setup)
                .await
        }
        // A distro's server is installed and launched from here too, so both
        // moves mean the same thing they do over SSH — only the transport is
        // different.
        (RouteTarget::Wsl { distro }, RouteAction::ReplaceServer) => {
            let distro = distro.clone();
            setup
                .blocking(move || crate::daemon::install::wsl::replace_wsl_server(&distro))
                .await??;
            Ok(())
        }
        (RouteTarget::Wsl { distro }, _) => {
            let distro = distro.clone();
            setup
                .blocking(move || crate::daemon::install::wsl::restart_wsl_daemon(&distro))
                .await??;
            Ok(())
        }
        _ => Err(anyhow::anyhow!(
            "restarting tty7's server is only supported for machines it serves, not {}",
            header.describe()
        )),
    }
}

async fn open_link(
    header: &RouteHeader,
    setup: &RouteSetup,
) -> anyhow::Result<(RemoteLink, Option<Arc<SshConnection>>)> {
    match &header.target {
        RouteTarget::Ssh(spec) => {
            let (link, conn) = SshManager::global()
                .open_remote_link(spec, setup, header.server_command.as_deref())
                .await?;
            Ok((link, Some(conn)))
        }
        RouteTarget::Wsl { distro } => {
            let resolved = match header.server_command {
                Some(_) => None,
                None => {
                    let distro = distro.clone();
                    Some(
                        setup
                            .blocking(move || {
                                crate::daemon::install::wsl::ensure_wsl_server(&distro)
                            })
                            .await??,
                    )
                }
            };
            let link = match (header.server_command.as_deref(), resolved.as_deref()) {
                (Some(command), _) => RemoteLink::wsl_shell(distro, command, setup.channel)?,
                (None, Some(binary)) => RemoteLink::wsl(distro, binary, setup.channel)?,
                (None, None) => unreachable!("resolved is Some whenever there is no override"),
            };
            Ok((link, None))
        }
        RouteTarget::LocalStdio { program, args } => {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            Ok((RemoteLink::local_stdio(program, &args)?, None))
        }
    }
}

#[cfg(unix)]
fn into_async(local: Stream) -> io::Result<tokio::net::UnixStream> {
    local.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(local)
}

#[cfg(windows)]
fn into_async(local: Stream) -> io::Result<tokio::net::TcpStream> {
    local.set_nonblocking(true)?;
    tokio::net::TcpStream::from_std(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_round_trips_through_a_frame() {
        let header = RouteHeader::local_stdio("cat", &["-u"]);
        let mut buf = Vec::new();
        header.write(&mut buf).unwrap();

        let (kind, payload) = protocol::read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(kind, ROUTE_KIND);
        let back = RouteHeader::decode(&payload).unwrap();
        match back.target {
            RouteTarget::LocalStdio { program, args } => {
                assert_eq!(program, "cat");
                assert_eq!(args, vec!["-u".to_string()]);
            }
            other => panic!("wrong target: {other:?}"),
        }
        assert_eq!(back.server_command, None);
    }

    #[test]
    fn a_wsl_header_round_trips_with_only_a_distro_name() {
        let mut buf = Vec::new();
        RouteHeader::wsl("Ubuntu-22.04").write(&mut buf).unwrap();
        let (kind, payload) = protocol::read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(kind, ROUTE_KIND);
        let back = RouteHeader::decode(&payload).unwrap();
        match &back.target {
            RouteTarget::Wsl { distro } => assert_eq!(distro, "Ubuntu-22.04"),
            other => panic!("wrong target: {other:?}"),
        }
        assert_eq!(back.server_command, None);
        assert_eq!(back.describe(), "wsl Ubuntu-22.04");

        let json = String::from_utf8(payload).unwrap();
        assert!(json.contains(r#""wsl""#), "{json}");
    }

    #[tokio::test]
    async fn a_wsl_route_refuses_a_distro_name_that_could_be_an_option() {
        for header in [
            RouteHeader::wsl("--shutdown"),
            RouteHeader {
                target: RouteTarget::Wsl {
                    distro: "--shutdown".to_string(),
                },
                server_command: Some("tty7-server --stdio".to_string()),
                channel: RouteChannel::Control,
                action: RouteAction::Forward,
            },
        ] {
            let describe = header.describe();
            let setup = RouteSetup::unattended(header.channel);
            let Err(err) = open_link(&header, &setup).await else {
                panic!("a name starting with `-` must be refused ({describe})");
            };
            let msg = err.to_string();
            assert!(msg.contains("leading `-`"), "{describe}: {msg}");
        }
    }

    #[test]
    fn a_failed_route_reports_why() {
        let mut buf = Vec::new();
        RouteAck::failed("no such host".into())
            .write(&mut buf)
            .unwrap();
        let err = RouteAck::read(&mut buf.as_slice()).expect_err("should surface the failure");
        assert!(err.to_string().contains("no such host"), "{err}");
    }

    #[tokio::test]
    async fn a_successful_ack_names_the_transport() {
        let link = RemoteLink::local_stdio("cat", &[]).unwrap();
        let mut buf = Vec::new();
        RouteAck::ok(&link).write(&mut buf).unwrap();
        let ack = RouteAck::read(&mut buf.as_slice()).unwrap();
        assert_eq!(ack.link.as_deref(), Some("local-stdio"));
    }

    #[test]
    fn a_daemons_error_frame_is_surfaced_verbatim() {
        let mut buf = Vec::new();
        DaemonMsg::Error("unknown ClientMsg kind 51".into())
            .encode(&mut buf)
            .unwrap();
        let err = RouteAck::read(&mut buf.as_slice()).expect_err("not an ack");
        assert!(
            err.to_string().contains("unknown ClientMsg kind 51"),
            "{err}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_router_forwards_bytes_it_cannot_parse() {
        use std::io::Read as _;
        use std::os::unix::net::UnixStream;

        let (client, daemon_side) = UnixStream::pair().unwrap();
        let header = RouteHeader::local_stdio("cat", &[]);
        let routed = std::thread::spawn(move || RemoteRouter::route(daemon_side, &header));

        let mut client_read = client.try_clone().unwrap();
        let mut client_write = client;
        let ack = RouteAck::read(&mut client_read).expect("routed");
        assert_eq!(ack.link.as_deref(), Some("local-stdio"));

        let mut garbage: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff, 0xfe, 0x00, 0x80, 0xc3, 0x28];
        garbage.extend((0..512 * 1024u32).map(|i| (i % 256) as u8));

        let expected = garbage.clone();
        let writer = std::thread::spawn(move || {
            client_write.write_all(&garbage).unwrap();
            client_write.shutdown(std::net::Shutdown::Write).unwrap();
        });

        let mut got = Vec::new();
        client_read.read_to_end(&mut got).unwrap();
        writer.join().unwrap();
        assert_eq!(got.len(), expected.len(), "byte count changed in transit");
        assert!(got == expected, "bytes changed in transit");

        routed.join().unwrap().expect("clean close");
    }

    #[test]
    #[cfg(unix)]
    fn an_unopenable_link_fails_the_route() {
        use std::os::unix::net::UnixStream;

        let (client, daemon_side) = UnixStream::pair().unwrap();
        let header = RouteHeader::local_stdio("tty7-no-such-binary-anywhere", &[]);
        let routed = std::thread::spawn(move || RemoteRouter::route(daemon_side, &header));

        let mut client = client;
        let err = RouteAck::read(&mut client).expect_err("nothing to route to");
        assert!(!err.to_string().is_empty());
        assert!(routed.join().unwrap().is_err());
    }

    fn responder_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn a_request() -> InstallRequest {
        InstallRequest {
            host: "me@build-box:22".into(),
            version: "26.7.5".into(),
            asset: crate::daemon::install::asset::ASSET_AARCH64,
            source_url: "https://example.invalid/tty7-server".into(),
            remote_path: "/home/me/.local/share/tty7/bin/tty7-server-26.7.5".into(),
            size_bytes: 12_345_678,
            sha256: "abc123".into(),
        }
    }

    #[test]
    fn an_install_request_round_trips_through_a_prompt() {
        let original = a_request();
        let prompt = RoutePrompt::Install {
            request_id: 7,
            request: Box::new(InstallRequestWire::from_request(&original)),
        };
        let mut buf = Vec::new();
        prompt.write(&mut buf).unwrap();

        let (kind, payload) = protocol::read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(kind, ROUTE_PROMPT_KIND);
        match RoutePrompt::decode(&payload).unwrap() {
            RoutePrompt::Install {
                request_id,
                request,
            } => {
                assert_eq!(request_id, 7);
                assert_eq!(request.into_request(), original);
            }
            other => panic!("wrong prompt: {other:?}"),
        }
    }

    #[test]
    fn the_relay_turns_a_consent_question_into_a_frame_and_back() {
        let (out, mut outbox) = tokio::sync::mpsc::unbounded_channel();
        let relay = Arc::new(Relay {
            out,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });

        let asking = {
            let relay = relay.clone();
            std::thread::spawn(move || relay.confirm(&a_request()))
        };

        let (kind, payload) = outbox.blocking_recv().expect("a question went out");
        assert_eq!(kind, ROUTE_PROMPT_KIND);
        let RoutePrompt::Install { request_id, .. } = RoutePrompt::decode(&payload).unwrap() else {
            panic!("expected an install prompt");
        };
        relay.fulfil(request_id, true);

        assert_eq!(asking.join().unwrap(), InstallDecision::Approve);
    }

    #[test]
    fn an_unanswerable_consent_question_declines() {
        let (out, outbox) = tokio::sync::mpsc::unbounded_channel();
        drop(outbox);
        let relay = Relay {
            out,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        };
        assert_eq!(relay.confirm(&a_request()), InstallDecision::Decline);
    }

    #[test]
    #[cfg(unix)]
    fn negotiate_answers_a_consent_question_and_then_takes_the_ack() {
        use std::os::unix::net::UnixStream;

        struct Approve;
        impl InstallConfirm for Approve {
            fn confirm(&self, request: &InstallRequest) -> InstallDecision {
                assert_eq!(request.host, "me@build-box:22");
                assert_eq!(request.asset, crate::daemon::install::asset::ASSET_AARCH64);
                InstallDecision::Approve
            }
        }

        let (client, daemon) = UnixStream::pair().unwrap();

        let daemon = std::thread::spawn(move || {
            let mut daemon = daemon;
            let (kind, payload) = protocol::read_frame(&mut daemon).unwrap();
            assert_eq!(kind, ROUTE_KIND, "the header comes first");
            assert_eq!(
                RouteHeader::decode(&payload).unwrap().channel,
                RouteChannel::Pane
            );

            RoutePrompt::Install {
                request_id: 3,
                request: Box::new(InstallRequestWire::from_request(&a_request())),
            }
            .write(&mut daemon)
            .unwrap();

            let (kind, payload) = protocol::read_frame(&mut daemon).unwrap();
            assert_eq!(kind, ROUTE_REPLY_KIND);
            let answered = match RouteReply::decode(&payload).unwrap() {
                RouteReply::Install {
                    request_id,
                    approve,
                } => (request_id, approve),
                other => panic!("wrong reply: {other:?}"),
            };

            RouteAck {
                ok: true,
                link: Some("local-stdio".into()),
                action: Some(RouteAction::Forward),
                error: None,
            }
            .write(&mut daemon)
            .unwrap();
            answered
        });

        let mut client = client;
        let ack = crate::daemon::install::with_install_confirm(Arc::new(Approve), || {
            negotiate(&mut client, &RouteHeader::local_stdio("x", &[]).for_pane())
        })
        .expect("the route is acked after the question is answered");
        assert_eq!(ack.link.as_deref(), Some("local-stdio"));
        assert_eq!(daemon.join().unwrap(), (3, true));
    }

    #[test]
    #[cfg(unix)]
    fn negotiate_answers_an_auth_question() {
        use std::os::unix::net::UnixStream;

        struct Typed;
        impl RouteAuthResponder for Typed {
            fn respond(&self, machine: &RouteTarget, prompt: &AuthPromptKind) -> AuthResponse {
                assert_eq!(machine.origin_key(), "local-stdio:x");
                assert!(matches!(prompt, AuthPromptKind::Password { .. }));
                AuthResponse::Secret("hunter2".into())
            }
        }

        let (client, daemon) = UnixStream::pair().unwrap();
        let daemon = std::thread::spawn(move || {
            let mut daemon = daemon;
            let _ = protocol::read_frame(&mut daemon).unwrap();
            RoutePrompt::Auth {
                request_id: 11,
                prompt: AuthPromptKind::Password {
                    user: "me".into(),
                    host: "build-box".into(),
                },
            }
            .write(&mut daemon)
            .unwrap();
            let (_, payload) = protocol::read_frame(&mut daemon).unwrap();
            let reply = RouteReply::decode(&payload).unwrap();
            RouteAck {
                ok: true,
                link: Some("session-exec".into()),
                action: Some(RouteAction::Forward),
                error: None,
            }
            .write(&mut daemon)
            .unwrap();
            reply
        });

        let _serialized = responder_lock();
        set_route_auth_responder(Arc::new(Typed));
        let mut client = client;
        negotiate(&mut client, &RouteHeader::local_stdio("x", &[])).expect("acked");
        set_route_auth_responder(Arc::new(CancelAuth));

        match daemon.join().unwrap() {
            RouteReply::Auth {
                request_id,
                response,
            } => {
                assert_eq!(request_id, 11);
                assert!(matches!(response, AuthResponse::Secret(s) if s == "hunter2"));
            }
            other => panic!("wrong reply: {other:?}"),
        }
    }

    #[test]
    fn the_default_auth_responder_cancels() {
        assert!(matches!(
            CancelAuth.respond(
                &RouteTarget::Wsl {
                    distro: "Ubuntu".into()
                },
                &AuthPromptKind::Password {
                    user: "u".into(),
                    host: "h".into(),
                }
            ),
            AuthResponse::Cancelled
        ));
    }

    #[test]
    fn a_scoped_mismatch_sink_diverts_the_record() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let entry = MismatchedRemoteDaemon {
            host: "me@scoped-box:22".into(),
            running_version: Some("26.7.4".into()),
            running_exe: None,
            wanted_version: "26.7.5".into(),
        };
        crate::daemon::install::with_mismatch_sink(sink.clone(), || {
            crate::daemon::install::record_remote_mismatches(vec![entry.clone(), entry.clone()])
        });
        let found = sink.lock().unwrap().clone();
        assert_eq!(
            found,
            vec![entry],
            "one entry per host, as the registry does"
        );
    }

    #[test]
    fn a_scoped_confirm_handler_outranks_the_global_and_restores() {
        struct Yes;
        impl InstallConfirm for Yes {
            fn confirm(&self, _: &InstallRequest) -> InstallDecision {
                InstallDecision::Approve
            }
        }
        let before = crate::daemon::install::install_confirm().confirm(&a_request());
        let inside = crate::daemon::install::with_install_confirm(Arc::new(Yes), || {
            crate::daemon::install::install_confirm().confirm(&a_request())
        });
        let after = crate::daemon::install::install_confirm().confirm(&a_request());
        assert_eq!(inside, InstallDecision::Approve);
        assert_eq!(after, before, "the previous handler is back");
    }

    #[test]
    fn the_channel_defaults_to_control_and_survives_the_wire() {
        let header = RouteHeader::local_stdio("cat", &[]);
        assert_eq!(header.channel, RouteChannel::Control);

        let mut buf = Vec::new();
        header.clone().for_pane().write(&mut buf).unwrap();
        let (_, payload) = protocol::read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(
            RouteHeader::decode(&payload).unwrap().channel,
            RouteChannel::Pane
        );

        let legacy = br#"{"target":{"wsl":{"distro":"Ubuntu"}}}"#;
        assert_eq!(
            RouteHeader::decode(legacy).unwrap().channel,
            RouteChannel::Control
        );
    }

    #[test]
    fn the_action_defaults_to_forward_and_survives_the_wire() {
        let header = RouteHeader::local_stdio("cat", &[]);
        assert_eq!(header.action, RouteAction::Forward);

        let mut buf = Vec::new();
        header.clone().restart_server().write(&mut buf).unwrap();
        let (_, payload) = protocol::read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(
            RouteHeader::decode(&payload).unwrap().action,
            RouteAction::RestartServer
        );
        assert!(
            String::from_utf8(payload)
                .unwrap()
                .contains("restart_server"),
            "the action's wire tag changed"
        );

        let legacy = br#"{"target":{"wsl":{"distro":"Ubuntu"}},"channel":"pane"}"#;
        let back = RouteHeader::decode(legacy).unwrap();
        assert_eq!(back.action, RouteAction::Forward);
        assert_eq!(back.channel, RouteChannel::Pane, "and nothing else moved");
    }

    #[test]
    fn an_ack_without_an_action_is_not_a_restart() {
        let legacy = br#"{"ok":true,"link":"session-exec"}"#;
        let ack: RouteAck = serde_json::from_slice(legacy).unwrap();
        assert_eq!(ack.action, None);
        assert!(!ack.performed(RouteAction::RestartServer));

        assert!(RouteAck::acted(RouteAction::RestartServer).performed(RouteAction::RestartServer));
        let forwarded = RouteAck {
            ok: true,
            link: Some("session-exec".into()),
            action: Some(RouteAction::Forward),
            error: None,
        };
        assert!(!forwarded.performed(RouteAction::RestartServer));
        assert!(forwarded.performed(RouteAction::Forward));
    }

    /// SSH and WSL machines both run a daemon this side installed, so both can
    /// be restarted. A `--stdio` program is whatever the user named — there is
    /// no daemon of ours behind it to stop.
    #[tokio::test]
    async fn a_restart_is_refused_for_a_machine_that_has_no_remote_daemon() {
        let header = RouteHeader::local_stdio("cat", &[]).restart_server();
        let describe = header.describe();
        let setup = RouteSetup::unattended(header.channel);
        let Err(err) = perform(&header, &setup).await else {
            panic!("a restart must be refused for {describe}");
        };
        assert!(
            err.to_string()
                .contains("only supported for machines it serves"),
            "{err}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_restart_route_answers_and_closes_without_forwarding() {
        use std::os::unix::net::UnixStream;

        let (client, daemon_side) = UnixStream::pair().unwrap();
        let header = RouteHeader::local_stdio("cat", &[]).restart_server();
        let routed = std::thread::spawn(move || RemoteRouter::route(daemon_side, &header));

        let mut client = client;
        let err = RouteAck::read(&mut client).expect_err("a `cat` has no daemon to restart");
        assert!(
            err.to_string()
                .contains("only supported for machines it serves"),
            "{err}"
        );
        assert!(routed.join().unwrap().is_err());
    }

    #[test]
    fn the_origin_key_of_an_ssh_target_is_its_connection_key() {
        let spec: NativeSshSpec = serde_json::from_str(
            r#"{"user":"me","host":"build-box","port":2222,"auth_mode":"agent"}"#,
        )
        .expect("a minimal spec");
        let target = RouteTarget::Ssh(Box::new(spec.clone()));
        assert_eq!(
            target.origin_key(),
            crate::daemon::ssh::ConnectionKey::from_spec(&spec)
                .as_str()
                .to_string()
        );

        let control = RouteTarget::LocalStdio {
            program: "/opt/tty7-server".into(),
            args: vec!["--stdio".into()],
        };
        let pane = RouteTarget::LocalStdio {
            program: "/opt/tty7-server".into(),
            args: vec!["--stdio".into(), "--pane".into()],
        };
        assert_eq!(control.origin_key(), pane.origin_key());
        assert_ne!(
            control.origin_key(),
            RouteTarget::Wsl {
                distro: "Ubuntu".into()
            }
            .origin_key()
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_relayed_prompt_names_the_machine_from_the_header() {
        use std::os::unix::net::UnixStream;

        #[derive(Default)]
        struct Recorder(Mutex<Vec<String>>);
        impl RouteAuthResponder for Recorder {
            fn respond(&self, machine: &RouteTarget, _: &AuthPromptKind) -> AuthResponse {
                self.0.lock().unwrap().push(machine.origin_key());
                AuthResponse::Cancelled
            }
        }

        let (client, daemon) = UnixStream::pair().unwrap();
        let daemon = std::thread::spawn(move || {
            let mut daemon = daemon;
            let _ = protocol::read_frame(&mut daemon).unwrap();
            RoutePrompt::Auth {
                request_id: 1,
                prompt: AuthPromptKind::Password {
                    user: "me".into(),
                    host: "build-box".into(),
                },
            }
            .write(&mut daemon)
            .unwrap();
            let _ = protocol::read_frame(&mut daemon).unwrap();
            RouteAck {
                ok: true,
                link: Some("wsl".into()),
                action: Some(RouteAction::Forward),
                error: None,
            }
            .write(&mut daemon)
            .unwrap();
        });

        let _serialized = responder_lock();
        let recorder = Arc::new(Recorder::default());
        set_route_auth_responder(recorder.clone());
        let mut client = client;
        negotiate(&mut client, &RouteHeader::wsl("Ubuntu-22.04").for_pane()).expect("acked");
        set_route_auth_responder(Arc::new(CancelAuth));
        daemon.join().unwrap();

        assert_eq!(recorder.0.lock().unwrap().as_slice(), ["wsl:Ubuntu-22.04"]);
    }

    #[test]
    fn only_the_pane_channel_changes_the_bridge_command() {
        let base = crate::daemon::remote_link::DEFAULT_REMOTE_SERVER_CMD;
        assert_eq!(RouteChannel::Control.bridge_command(base), base);
        assert_eq!(
            RouteChannel::Pane.bridge_command(base),
            format!("{base} --pane")
        );
    }
}

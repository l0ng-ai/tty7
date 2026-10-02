//! The phone side of the tty7 mobile link.
//!
//! The app holds one iroh [`Endpoint`] for its whole life — its key is the
//! phone's identity, the thing a gateway put on its device list at pairing — and
//! a [`Session`] per machine it is looking at. Everything here is transport:
//! what to draw, and when to reconnect, is the app's business.

use std::str::FromStr as _;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use iroh::endpoint::{Connection, QuicTransportConfig, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl, SecretKey};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use serde::{Deserialize, Serialize};
use tty7_mobile_proto::{
    ALPN, ControlEvent, ControlRequest, Diff, Frame, GridSize, MAX_UPLOAD, MDNS_SERVICE, Open,
    OpenReply, PROTOCOL_VERSION, PairCode, PaneEvent, PaneRequest, TabCreated, Uploaded,
    read_frame, write_bytes, write_msg,
};

/// How long to wait for a gateway to answer an [`Open`].
const OPEN_REPLY_WAIT: Duration = Duration::from_secs(15);

/// A machine this phone has paired with, as the app stores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// The gateway's endpoint id: the key the phone trusts it by.
    pub id: String,
    pub name: String,
    /// Where it was last known to be. A head start for the dial, never a
    /// requirement: iroh looks the id up if these have gone stale.
    #[serde(default)]
    pub relay: Option<String>,
    #[serde(default)]
    pub addrs: Vec<String>,
}

impl Host {
    fn addr(&self) -> Result<EndpointAddr> {
        let id = EndpointId::from_str(&self.id)
            .context("this machine's pairing is damaged — pair it again")?;
        let mut addr = EndpointAddr::new(id);
        if let Some(relay) = self
            .relay
            .as_deref()
            .and_then(|r| RelayUrl::from_str(r).ok())
        {
            addr = addr.with_relay_url(relay);
        }
        for ip in self.addrs.iter().filter_map(|a| a.parse().ok()) {
            addr = addr.with_ip_addr(ip);
        }
        Ok(addr)
    }
}

/// Binds the phone's endpoint with iroh's public relays and address lookup,
/// plus mDNS: on the gateway's own network that finds it by key even when the
/// addresses the phone knows are stale and no relay can be reached. The phone
/// only listens; it has no reason to announce itself. Where multicast is not
/// allowed it does without.
pub async fn bind(secret: SecretKey) -> Result<Endpoint> {
    let mdns = MdnsAddressLookup::builder()
        .service_name(MDNS_SERVICE)
        .advertise(false);
    match Endpoint::builder(presets::N0)
        .secret_key(secret.clone())
        .transport_config(transport())
        .address_lookup(mdns)
        .bind()
        .await
    {
        Ok(endpoint) => Ok(endpoint),
        Err(_) => Endpoint::builder(presets::N0)
            .secret_key(secret)
            .transport_config(transport())
            .bind()
            .await
            .context("could not start the connection"),
    }
}

/// How long a machine may go unheard before its connection counts as gone.
/// Keep-alives go every few seconds, so this is a few of them missed: a
/// laptop that went to sleep shows as unreachable within seconds, not after
/// half a minute of the app saying the link is fine.
const IDLE_TIMEOUT: Duration = Duration::from_secs(12);

fn transport() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .max_idle_timeout(IDLE_TIMEOUT.try_into().ok())
        .build()
}

/// Trades a pairing code for a [`Host`] the app can store and dial later.
pub async fn pair(endpoint: &Endpoint, code: &str, device_name: &str) -> Result<Host> {
    let code = PairCode::decode(code).map_err(|e| anyhow!(e))?;
    let host = Host {
        id: code.host_id,
        name: code.host_name,
        relay: code.relay,
        addrs: code.addrs,
    };
    let conn = endpoint
        .connect(host.addr()?, ALPN)
        .await
        .with_context(|| format!("could not reach {}", host.name))?;
    let (mut send, mut recv) = open(
        &conn,
        &Open::Pair {
            secret: code.secret,
            device_name: device_name.to_string(),
        },
    )
    .await?;
    let _ = send.finish();
    let _ = read_frame(&mut recv).await;
    conn.close(0u32.into(), b"paired");
    Ok(host)
}

/// A live connection to one machine.
#[derive(Debug, Clone)]
pub struct Session {
    conn: Connection,
}

/// How the connection is getting through right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkInfo {
    /// `direct` once hole punching has worked, `relay` until then (or for
    /// good, on networks that block it).
    pub path: String,
    pub rtt_ms: u64,
}

impl Session {
    pub async fn connect(endpoint: &Endpoint, host: &Host) -> Result<Session> {
        let conn = endpoint
            .connect(host.addr()?, ALPN)
            .await
            .with_context(|| format!("could not reach {}", host.name))?;
        Ok(Session { conn })
    }

    pub fn link(&self) -> LinkInfo {
        let paths = self.conn.paths();
        let selected = paths.iter().find(|p| p.is_selected());
        LinkInfo {
            path: match &selected {
                Some(p) if p.is_ip() => "direct",
                Some(_) => "relay",
                None => "connecting",
            }
            .to_string(),
            rtt_ms: selected.map_or(0, |p| p.rtt().as_millis() as u64),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.conn.close_reason().is_some()
    }

    pub fn close(&self) {
        self.conn.close(0u32.into(), b"bye");
    }

    /// Subscribes to the machine's tree.
    pub async fn control(&self) -> Result<ControlStream> {
        let (send, recv) = open(&self.conn, &Open::Control).await?;
        Ok(ControlStream { send, recv })
    }

    /// Opens one pane: its output (replay first) arrives on the reader, and
    /// keystrokes go in through the writer. `machine` is the key of the
    /// remote it lives on, `None` for the gateway's own machine.
    pub async fn pane(
        &self,
        machine: Option<&str>,
        pane_id: u64,
    ) -> Result<(PaneWriter, PaneReader)> {
        let ask = Open::Pane {
            pane_id,
            machine: machine.map(str::to_string),
        };
        let (send, recv) = open(&self.conn, &ask).await?;
        Ok((PaneWriter { send }, PaneReader { recv }))
    }
}

impl Session {
    /// Starts a shell in a new tab at the end of a workspace, returning the
    /// pane to open with [`Session::pane`].
    pub async fn new_tab(
        &self,
        machine: Option<&str>,
        workspace_id: &str,
        cwd: Option<String>,
        size: Option<GridSize>,
    ) -> Result<TabCreated> {
        let ask = Open::NewTab {
            workspace_id: workspace_id.to_string(),
            cwd,
            size,
            machine: machine.map(str::to_string),
        };
        // A gateway from before new tabs cannot parse the ask and drops the
        // stream without a word; say what that means rather than that it hung
        // up.
        let (mut send, mut recv) = open(&self.conn, &ask).await.map_err(|e| {
            if e.to_string().contains("without answering") {
                anyhow!("tty7 on this computer is too old to open tabs — update it")
            } else {
                e
            }
        })?;
        let _ = send.finish();
        let created = read_frame(&mut recv)
            .await?
            .ok_or_else(|| anyhow!("the machine did not say which tab it opened"))?
            .msg::<TabCreated>()?;
        Ok(created)
    }
}

impl Session {
    /// Closes a tab on `machine`, its panes with it.
    pub async fn close_tab(
        &self,
        machine: Option<&str>,
        workspace_id: &str,
        tab_id: &str,
    ) -> Result<()> {
        let ask = Open::CloseTab {
            workspace_id: workspace_id.to_string(),
            tab_id: tab_id.to_string(),
            machine: machine.map(str::to_string),
        };
        let (mut send, _) = open(&self.conn, &ask).await.map_err(|e| {
            if e.to_string().contains("without answering") {
                anyhow!("tty7 on this computer is too old to close tabs — update it")
            } else {
                e
            }
        })?;
        let _ = send.finish();
        Ok(())
    }
}

impl Session {
    /// Sends a file to the machine for a pane to be handed, returning where
    /// it landed. `machine` is where that pane runs, as on [`Session::pane`].
    pub async fn upload(&self, machine: Option<&str>, name: &str, bytes: &[u8]) -> Result<String> {
        if bytes.len() as u64 > MAX_UPLOAD {
            bail!(
                "that file is {} MB; files up to {} MB can be sent",
                (bytes.len() as u64).div_ceil(1 << 20),
                MAX_UPLOAD >> 20
            );
        }
        let ask = Open::Upload {
            name: name.to_string(),
            size: bytes.len() as u64,
            machine: machine.map(str::to_string),
        };
        let (mut send, mut recv) = open(&self.conn, &ask).await.map_err(|e| {
            if e.to_string().contains("without answering") {
                anyhow!("tty7 on this computer is too old to take files — update it")
            } else {
                e
            }
        })?;
        for chunk in bytes.chunks(UPLOAD_CHUNK) {
            write_bytes(&mut send, chunk).await?;
        }
        let _ = send.finish();
        let answer = read_frame(&mut recv)
            .await?
            .ok_or_else(|| anyhow!("the machine did not say where the file went"))?;
        if let Ok(OpenReply::Denied { reason }) = answer.msg::<OpenReply>() {
            bail!(reason);
        }
        Ok(answer.msg::<Uploaded>()?.path)
    }
}

impl Session {
    /// What has changed in the repository `cwd` is in, on `machine`.
    pub async fn diff(&self, machine: Option<&str>, cwd: &str) -> Result<Diff> {
        let ask = Open::Diff {
            cwd: cwd.to_string(),
            machine: machine.map(str::to_string),
        };
        let (mut send, mut recv) = open(&self.conn, &ask).await.map_err(|e| {
            if e.to_string().contains("without answering") {
                anyhow!("tty7 on this computer is too old to show changes — update it")
            } else {
                e
            }
        })?;
        let _ = send.finish();
        let diff = read_frame(&mut recv)
            .await?
            .ok_or_else(|| anyhow!("the machine did not send the changes"))?
            .msg::<Diff>()?;
        Ok(diff)
    }
}

/// How much of a file goes in one frame: well under the frame limit, and small
/// enough that a slow link shows progress rather than one long wait.
const UPLOAD_CHUNK: usize = 256 << 10;

async fn open(conn: &Connection, open: &Open) -> Result<(SendStream, RecvStream)> {
    let (mut send, mut recv) = conn.open_bi().await.context("opening a stream")?;
    write_msg(&mut send, open).await?;
    let reply = tokio::time::timeout(OPEN_REPLY_WAIT, read_frame(&mut recv))
        .await
        .map_err(|_| anyhow!("the machine did not answer"))??
        .ok_or_else(|| anyhow!("the machine closed the stream without answering"))?;
    match reply.msg::<OpenReply>()? {
        OpenReply::Ok { version, .. } if version != PROTOCOL_VERSION => bail!(
            "this machine's gateway speaks protocol {version}, the app speaks \
             {PROTOCOL_VERSION} — update whichever is older"
        ),
        OpenReply::Ok { .. } => Ok((send, recv)),
        OpenReply::Denied { reason } => bail!(reason),
    }
}

pub struct ControlStream {
    send: SendStream,
    recv: RecvStream,
}

impl ControlStream {
    /// Splits into a half that asks for refreshes and a half that reads the
    /// tree, so each can live on its own task.
    pub fn split(self) -> (ControlSender, ControlReceiver) {
        (
            ControlSender { send: self.send },
            ControlReceiver { recv: self.recv },
        )
    }
}

pub struct ControlSender {
    send: SendStream,
}

impl ControlSender {
    pub async fn refresh(&mut self) -> Result<()> {
        write_msg(&mut self.send, &ControlRequest::Refresh).await?;
        Ok(())
    }
}

pub struct ControlReceiver {
    recv: RecvStream,
}

impl ControlReceiver {
    pub async fn next(&mut self) -> Result<Option<ControlEvent>> {
        match read_frame(&mut self.recv).await? {
            Some(frame) => Ok(Some(frame.msg()?)),
            None => Ok(None),
        }
    }
}

pub struct PaneWriter {
    send: SendStream,
}

impl PaneWriter {
    pub async fn input(&mut self, bytes: &[u8]) -> Result<()> {
        write_bytes(&mut self.send, bytes).await?;
        Ok(())
    }

    pub async fn request(&mut self, request: &PaneRequest) -> Result<()> {
        write_msg(&mut self.send, request).await?;
        Ok(())
    }

    pub fn close(mut self) {
        let _ = self.send.finish();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaneItem {
    Output(Vec<u8>),
    Event(PaneEvent),
}

pub struct PaneReader {
    recv: RecvStream,
}

impl PaneReader {
    /// The next output chunk or event, `None` once the stream has ended.
    pub async fn next(&mut self) -> Result<Option<PaneItem>> {
        match read_frame(&mut self.recv).await? {
            Some(Frame::Bytes(bytes)) => Ok(Some(PaneItem::Output(bytes))),
            Some(frame) => Ok(Some(PaneItem::Event(frame.msg()?))),
            None => Ok(None),
        }
    }
}

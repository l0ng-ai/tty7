//! The gateway proper: accept phones over iroh, check who they are, and bridge
//! each stream they open to the daemon.
//!
//! The daemon's client library is blocking — one thread per connection, the
//! same model as the daemon itself — so every stream gets a thread for its
//! daemon side and a tokio task for its phone side, joined by a channel. The
//! channels are bounded: a phone that stops reading stalls its own thread, and
//! through it only its own daemon connection, never the gateway.

use std::io;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use iroh::Endpoint;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use tokio::sync::mpsc;
use tty7_core::core::cli_agent::{AgentSessionState, CLIAgent};
use tty7_core::core::machine::Machine;
use tty7_core::daemon::control::PaneAgentState;
use tty7_core::daemon::protocol::{DaemonMsg, LeaseRequest, WinSize};
use tty7_mobile_proto::{
    ControlEvent, ControlRequest, Frame, GridSize, Open, OpenReply, PROTOCOL_VERSION, PaneEvent,
    PaneRequest, RemoteView, TabCreated, Tree, read_frame, write_bytes, write_msg,
};

use crate::state::State;
use crate::tree;

/// How often the tree is re-read while a phone is watching it. Agent status
/// has no event on the control socket yet, so this is also how quickly a
/// "waiting for you" reaches the phone.
const TREE_POLL: Duration = Duration::from_secs(1);
/// How long a new stream has to say what it is for.
const OPEN_WAIT: Duration = Duration::from_secs(10);
/// How often a pane's reader thread looks up from the daemon to check whether
/// the phone is still there.
const FEED_TICK: Duration = Duration::from_millis(500);
/// Output chunks in flight per pane before the reader thread waits.
const PANE_BACKLOG: usize = 256;
/// A wrong pairing secret costs this long, which with single-use offers makes
/// guessing pointless as well as hopeless.
const PAIR_FAILURE_DELAY: Duration = Duration::from_secs(2);

/// What the gateway needs from the machine it serves. The daemon is the real
/// one; tests stand in a fake so they can drive the bridge without a PTY.
///
/// `machine` names a remote the desktop is linked to, by its link key, and is
/// `None` for this machine itself.
pub trait Backend: Send + Sync + 'static {
    fn hostname(&self) -> String;
    fn snapshot(&self) -> io::Result<(Machine, Vec<PaneAgentState>)>;
    /// Every machine the desktop is linked to, each read through its link.
    fn remotes(&self) -> Vec<Remote>;
    fn observe(&self, machine: Option<&str>, pane_id: u64) -> io::Result<Box<dyn PaneFeed>>;
    fn send_input(&self, machine: Option<&str>, pane_id: u64, bytes: &[u8]) -> io::Result<()>;
    /// Starts a shell in a new tab at the end of `workspace_id`.
    fn new_tab(
        &self,
        machine: Option<&str>,
        workspace_id: &str,
        cwd: Option<String>,
        size: Option<GridSize>,
    ) -> io::Result<TabCreated>;
}

/// One linked machine, as [`Backend::remotes`] reports it.
pub struct Remote {
    pub key: String,
    pub name: String,
    pub connected: bool,
    /// Its tree, read through the link: `None` while the link is down, or
    /// while it is up but has not answered yet.
    pub snapshot: Option<io::Result<(Machine, Vec<PaneAgentState>)>>,
}

/// A read-only view onto one pane's output, as the daemon sends it.
pub trait PaneFeed: Send {
    /// The next message, or `Ok(None)` if none arrived within `wait`.
    fn recv(&mut self, wait: Duration) -> io::Result<Option<DaemonMsg>>;

    /// The way back to the daemon on the same connection, for a take-over.
    /// Taken once, before the feed goes to its reader thread.
    fn leases(&mut self) -> Option<Box<dyn PaneLeases>> {
        None
    }
}

/// Asks the daemon to run a pane at the phone's size, over the connection
/// that observes it, so the lease ends when that connection does.
pub trait PaneLeases: Send {
    /// Fails with `Unsupported` where the pane's daemon predates leases.
    fn send(&mut self, request: LeaseRequest) -> io::Result<()>;
}

pub async fn run(endpoint: Endpoint, state: State, backend: Arc<dyn Backend>) {
    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        let backend = backend.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => serve_connection(conn, state, backend).await,
                Err(e) => log::debug!("mobile gateway: handshake failed: {e}"),
            }
        });
    }
}

async fn serve_connection(conn: Connection, state: State, backend: Arc<dyn Backend>) {
    let peer = conn.remote_id().to_string();
    loop {
        match conn.accept_bi().await {
            Ok((send, recv)) => {
                let (peer, state, backend) = (peer.clone(), state.clone(), backend.clone());
                tokio::spawn(async move {
                    if let Err(e) = serve_stream(&peer, send, recv, state, backend).await {
                        log::debug!("mobile gateway: stream from {}: {e}", short(&peer));
                    }
                });
            }
            // The phone went away or closed the connection: its streams end
            // with it, and their threads notice at their next tick.
            Err(_) => return,
        }
    }
}

async fn serve_stream(
    peer: &str,
    mut send: SendStream,
    mut recv: RecvStream,
    state: State,
    backend: Arc<dyn Backend>,
) -> io::Result<()> {
    let open: Open = match tokio::time::timeout(OPEN_WAIT, read_frame(&mut recv)).await {
        Ok(Ok(Some(frame))) => frame.msg()?,
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "no Open")),
    };
    let ok = OpenReply::Ok {
        host: backend.hostname(),
        version: PROTOCOL_VERSION,
    };

    match open {
        Open::Pair {
            secret,
            device_name,
        } => {
            if state.take_pairing(&secret) {
                state
                    .add_device(peer, &device_name)
                    .map_err(io::Error::other)?;
                log::info!("mobile gateway: paired {device_name} ({})", short(peer));
                write_msg(&mut send, &ok).await?;
            } else {
                tokio::time::sleep(PAIR_FAILURE_DELAY).await;
                write_msg(
                    &mut send,
                    &denied("that pairing code is used up or expired"),
                )
                .await?;
            }
            finish(send).await;
            Ok(())
        }
        _ if !state.is_paired(peer) => {
            write_msg(
                &mut send,
                &denied("this phone is not paired with this machine"),
            )
            .await?;
            finish(send).await;
            Ok(())
        }
        Open::Control => {
            write_msg(&mut send, &ok).await?;
            control_stream(send, recv, backend).await
        }
        Open::NewTab {
            workspace_id,
            cwd,
            size,
            machine,
        } => {
            let created = {
                let backend = backend.clone();
                tokio::task::spawn_blocking(move || {
                    backend.new_tab(machine.as_deref(), &workspace_id, cwd, size)
                })
                .await
                .map_err(io::Error::other)?
            };
            match created {
                Ok(created) => {
                    write_msg(&mut send, &ok).await?;
                    write_msg(&mut send, &created).await?;
                }
                Err(e) => write_msg(&mut send, &denied(&e.to_string())).await?,
            }
            finish(send).await;
            Ok(())
        }
        Open::Pane { pane_id, machine } => {
            let feed = {
                let (backend, machine) = (backend.clone(), machine.clone());
                tokio::task::spawn_blocking(move || backend.observe(machine.as_deref(), pane_id))
                    .await
                    .map_err(io::Error::other)?
            };
            match feed {
                Ok(feed) => {
                    write_msg(&mut send, &ok).await?;
                    // How the desktop names who has the pane.
                    let by = state
                        .devices()
                        .ok()
                        .and_then(|devices| devices.into_iter().find(|d| d.id == peer))
                        .map_or_else(|| "a phone".to_string(), |d| d.name);
                    pane_stream(machine, pane_id, by, feed, send, recv, backend).await
                }
                Err(e) => {
                    write_msg(&mut send, &denied(&e.to_string())).await?;
                    finish(send).await;
                    Ok(())
                }
            }
        }
    }
}

fn denied(reason: &str) -> OpenReply {
    OpenReply::Denied {
        reason: reason.to_string(),
    }
}

async fn finish(mut send: SendStream) {
    let _ = send.finish();
    // Give the peer a moment to read the last frame before the stream is
    // dropped, which would otherwise reset it and lose the reply.
    let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
}

async fn control_stream(
    mut send: SendStream,
    mut recv: RecvStream,
    backend: Arc<dyn Backend>,
) -> io::Result<()> {
    let (events_tx, mut events) = mpsc::channel::<ControlEvent>(4);
    let (refresh_tx, refresh_rx) = std_mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("gateway-tree".into())
        .spawn(move || watch_tree(backend, events_tx, refresh_rx))?;

    loop {
        tokio::select! {
            frame = read_frame(&mut recv) => match frame? {
                Some(frame) => match frame.msg::<ControlRequest>()? {
                    ControlRequest::Refresh => {
                        let _ = refresh_tx.send(());
                    }
                },
                None => return Ok(()),
            },
            event = events.recv() => match event {
                Some(event) => write_msg(&mut send, &event).await?,
                None => return Ok(()),
            },
        }
    }
}

/// Re-reads the tree every [`TREE_POLL`] and sends it when it changed, or at
/// once when the phone asks. Ends when the phone's side of the stream does.
fn watch_tree(
    backend: Arc<dyn Backend>,
    events: mpsc::Sender<ControlEvent>,
    refresh: std_mpsc::Receiver<()>,
) {
    let host = backend.hostname();
    let mut last: Option<Tree> = None;
    let mut failing = false;
    let mut forced = true;
    loop {
        if events.is_closed() {
            return;
        }
        let event = match backend.snapshot() {
            Ok((machine, agents)) => {
                failing = false;
                let mut tree = tree::build(&host, &machine, &agents);
                tree.remotes = backend.remotes().into_iter().map(remote_view).collect();
                if forced || last.as_ref() != Some(&tree) {
                    last = Some(tree.clone());
                    Some(ControlEvent::Tree(tree))
                } else {
                    None
                }
            }
            // Said once per outage, not once a second.
            Err(e) if !failing => {
                failing = true;
                last = None;
                Some(ControlEvent::Error {
                    message: server_down(&host, &e),
                })
            }
            Err(_) => None,
        };
        if let Some(event) = event
            && events.blocking_send(event).is_err()
        {
            return;
        }
        forced = match refresh.recv_timeout(TREE_POLL) {
            Ok(()) => true,
            Err(std_mpsc::RecvTimeoutError::Timeout) => false,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        };
    }
}

fn remote_view(remote: Remote) -> RemoteView {
    let pending = remote.connected && remote.snapshot.is_none();
    let (workspaces, error) = match remote.snapshot {
        Some(Ok((machine, agents))) => (
            tree::build(&remote.name, &machine, &agents).workspaces,
            None,
        ),
        Some(Err(e)) => (Vec::new(), Some(e.to_string())),
        None => (Vec::new(), None),
    };
    RemoteView {
        key: remote.key,
        name: remote.name,
        connected: remote.connected,
        error,
        pending,
        workspaces,
    }
}

/// What to say when the tty7 server can't be reached. No socket at all means
/// no server is running, which is the common case and has a fix to name —
/// opening the app, in the words of someone who uses it and never its CLI;
/// any other failure is passed on as it came.
pub fn server_down(host: &str, e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            format!("tty7 isn't running on {host} — open it there")
        }
        _ => format!("lost touch with tty7 on {host}: {e}"),
    }
}

enum Down {
    Bytes(Vec<u8>),
    Event(PaneEvent),
}

/// What the phone sends up a pane stream, in the order it sent it.
enum Up {
    Keys(Vec<u8>),
    Lease(PaneRequest),
}

async fn pane_stream(
    machine: Option<String>,
    pane_id: u64,
    by: String,
    mut feed: Box<dyn PaneFeed>,
    mut send: SendStream,
    mut recv: RecvStream,
    backend: Arc<dyn Backend>,
) -> io::Result<()> {
    let (down_tx, mut down) = mpsc::channel::<Down>(PANE_BACKLOG);
    // Weak, so the stream still ends when the pane's feed does.
    let refused = down_tx.downgrade();
    let mut leases = feed.leases();
    std::thread::Builder::new()
        .name(format!("gateway-pane-{pane_id}"))
        .spawn(move || read_pane(feed, down_tx))?;

    // Keystrokes go through one thread so they reach the pane in the order
    // they were typed; each `send_input` is its own daemon round trip. One
    // that fails is said to the phone, which would otherwise go on showing a
    // live pane while what it types goes nowhere. Take-overs share the
    // thread, so one sent after some keys lands after them.
    let (input_tx, input_rx) = std_mpsc::channel::<Up>();
    std::thread::Builder::new()
        .name(format!("gateway-input-{pane_id}"))
        .spawn(move || {
            let tell = |event: PaneEvent| {
                if let Some(down) = refused.upgrade() {
                    let _ = down.blocking_send(Down::Event(event));
                }
            };
            let mut typing = true;
            for up in input_rx {
                match up {
                    // Once typing has failed, the phone has been told; later
                    // keys are dropped rather than ending the stream, which
                    // would read as the pane closing.
                    Up::Keys(_) if !typing => {}
                    Up::Keys(bytes) => {
                        if let Err(e) = backend.send_input(machine.as_deref(), pane_id, &bytes) {
                            log::debug!("mobile gateway: input to pane {pane_id}: {e}");
                            typing = false;
                            tell(PaneEvent::Error {
                                message: format!("typing didn't reach the pane: {e}"),
                            });
                        }
                    }
                    Up::Lease(request) => {
                        let request = match request {
                            PaneRequest::TakeOver { size } => LeaseRequest::Take {
                                size: lease_size(size),
                                by: by.clone(),
                            },
                            PaneRequest::Release => LeaseRequest::Release,
                        };
                        let sent = match leases.as_mut() {
                            Some(leases) => leases.send(request),
                            None => Err(io::Error::new(
                                io::ErrorKind::Unsupported,
                                "this pane can't be taken over",
                            )),
                        };
                        // The daemon answers a lease itself, with who holds
                        // it; only a request that never got there is ours
                        // to answer.
                        if let Err(e) = sent {
                            tell(PaneEvent::Lease {
                                held: false,
                                refused: Some(e.to_string()),
                            });
                        }
                    }
                }
            }
        })?;

    loop {
        tokio::select! {
            frame = read_frame(&mut recv) => match frame? {
                Some(Frame::Bytes(bytes)) => {
                    let _ = input_tx.send(Up::Keys(bytes));
                }
                // A request this gateway does not know, from a newer app, is
                // ignored rather than fatal, so the terminal keeps working.
                Some(frame @ Frame::Json(_)) => {
                    if let Ok(request) = frame.msg::<PaneRequest>() {
                        let _ = input_tx.send(Up::Lease(request));
                    }
                }
                None => return Ok(()),
            },
            item = down.recv() => match item {
                Some(Down::Bytes(bytes)) => write_bytes(&mut send, &bytes).await?,
                Some(Down::Event(event)) => {
                    let last = matches!(event, PaneEvent::Exited { .. });
                    write_msg(&mut send, &event).await?;
                    if last {
                        finish(send).await;
                        return Ok(());
                    }
                }
                None => {
                    finish(send).await;
                    return Ok(());
                }
            },
        }
    }
}

fn read_pane(mut feed: Box<dyn PaneFeed>, down: mpsc::Sender<Down>) {
    // The daemon reports the agent and its status in separate messages; the
    // phone gets both together.
    let mut agent: Option<CLIAgent> = None;
    let mut status: Option<AgentSessionState> = None;
    loop {
        if down.is_closed() {
            return;
        }
        let msg = match feed.recv(FEED_TICK) {
            Ok(Some(msg)) => msg,
            Ok(None) => continue,
            Err(e) => {
                let _ = down.blocking_send(Down::Event(PaneEvent::Error {
                    message: e.to_string(),
                }));
                return;
            }
        };
        let item = match msg {
            DaemonMsg::Size(size) => Down::Event(PaneEvent::Size {
                cols: size.cols,
                rows: size.rows,
            }),
            DaemonMsg::Snapshot(bytes) | DaemonMsg::Output(bytes) => Down::Bytes(bytes),
            DaemonMsg::Cwd(path) => Down::Event(PaneEvent::Cwd {
                path: path.to_string_lossy().into_owned(),
            }),
            DaemonMsg::Agent(a) => {
                agent = a;
                Down::Event(agent_event(agent, status.as_ref()))
            }
            DaemonMsg::AgentStatus(s) => {
                status = s;
                Down::Event(agent_event(agent, status.as_ref()))
            }
            DaemonMsg::Exited { code } => {
                let _ = down.blocking_send(Down::Event(PaneEvent::Exited { code }));
                return;
            }
            DaemonMsg::Error(message) => Down::Event(PaneEvent::Error { message }),
            DaemonMsg::Lease(holder) => Down::Event(PaneEvent::Lease {
                held: holder.is_some(),
                refused: None,
            }),
            _ => continue,
        };
        if down.blocking_send(item).is_err() {
            return;
        }
    }
}

/// The phone's grid as the pty takes it, within sane bounds. Cell pixels are
/// nominal: nothing on the phone draws images at the pane's pixel size.
fn lease_size(size: GridSize) -> WinSize {
    WinSize {
        cols: size.cols.clamp(20, 500),
        rows: size.rows.clamp(5, 300),
        cell_w: 8,
        cell_h: 16,
    }
}

fn agent_event(agent: Option<CLIAgent>, status: Option<&AgentSessionState>) -> PaneEvent {
    PaneEvent::Agent {
        agent: agent.map(|agent| {
            let idle = AgentSessionState::default();
            tree::agent_view(agent, status.unwrap_or(&idle))
        }),
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(10)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_server_names_the_fix() {
        for kind in [io::ErrorKind::NotFound, io::ErrorKind::ConnectionRefused] {
            let message = server_down("studio", &io::Error::from(kind));
            assert!(message.contains("open it there"), "{message}");
            assert!(message.contains("studio"), "{message}");
        }
        let other = server_down("studio", &io::Error::other("bad dialect"));
        assert!(other.contains("bad dialect"), "{other}");
    }
}

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use russh::client::Msg;
use russh::{Channel, ChannelMsg};

use crate::daemon::install::ProvedServer;
use crate::daemon::protocol::WinSize;
use crate::daemon::remote_link::RemoteEntry;

use super::ConnectionKey;
use super::forward::RemoteForwardTable;

const DATA_CHANNEL_DEPTH: usize = 16;

/// How long a connection the server refused a session on is passed over when
/// the cache hands one out. Nothing tells us when one of its sessions closes,
/// so after this it is asked again: one refused round trip if it is still
/// full, a free slot if a pane on it has gone since.
const SATURATION_HOLD: Duration = Duration::from_secs(30);

pub type SharedConnection = Arc<Mutex<Weak<SshConnection>>>;

pub enum ChannelCmd {
    Data(Vec<u8>),
    Resize(WinSize),
    Close,
}

pub struct SshSessionHandle {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<ChannelCmd>,
}

impl SshSessionHandle {
    pub fn resize(&self, size: WinSize) {
        let _ = self.cmd_tx.send(ChannelCmd::Resize(size));
    }

    pub fn close(&self) {
        let _ = self.cmd_tx.send(ChannelCmd::Close);
    }

    fn send_data(&self, bytes: Vec<u8>) -> io::Result<()> {
        self.cmd_tx
            .send(ChannelCmd::Data(bytes))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ssh channel closed"))
    }
}

pub struct SshReader {
    rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    leftover: Vec<u8>,
    pos: usize,
}

impl Read for SshReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.leftover.len() {
            match self.rx.blocking_recv() {
                Some(data) if !data.is_empty() => {
                    self.leftover = data;
                    self.pos = 0;
                }
                Some(_) => continue,
                None => return Ok(0),
            }
        }
        let n = (self.leftover.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.leftover[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

pub struct SshWriter {
    handle: Arc<SshSessionHandle>,
}

impl Write for SshWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.handle.send_data(buf.to_vec())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct BridgeEnds {
    pub reader: SshReader,
    pub writer: SshWriter,
    pub handle: Arc<SshSessionHandle>,
    pub data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    pub cmd_rx: tokio::sync::mpsc::UnboundedReceiver<ChannelCmd>,
}

pub fn make_bridge() -> BridgeEnds {
    let (data_tx, data_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(DATA_CHANNEL_DEPTH);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ChannelCmd>();
    let handle = Arc::new(SshSessionHandle { cmd_tx });
    BridgeEnds {
        reader: SshReader {
            rx: data_rx,
            leftover: Vec::new(),
            pos: 0,
        },
        writer: SshWriter {
            handle: handle.clone(),
        },
        handle,
        data_tx,
        cmd_rx,
    }
}

fn pixels(size: WinSize) -> (u32, u32) {
    (
        u32::from(size.cols).saturating_mul(u32::from(size.cell_w)),
        u32::from(size.rows).saturating_mul(u32::from(size.cell_h)),
    )
}

pub async fn drive_channel(
    mut channel: Channel<Msg>,
    data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<ChannelCmd>,
    _conn: Arc<SshConnection>,
) {
    loop {
        tokio::select! {
            msg = channel.wait() => match msg {
                Some(ChannelMsg::Data { data }) => {
                    if data_tx.send(data.to_vec()).await.is_err() {
                        break;
                    }
                }
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    if data_tx.send(data.to_vec()).await.is_err() {
                        break;
                    }
                }
                Some(ChannelMsg::ExitStatus { .. }) | Some(ChannelMsg::ExitSignal { .. }) => {}
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(ChannelCmd::Data(bytes)) => {
                    let _ = channel.data(&bytes[..]).await;
                }
                Some(ChannelCmd::Resize(size)) => {
                    let (pw, ph) = pixels(size);
                    let _ = channel
                        .window_change(u32::from(size.cols), u32::from(size.rows), pw, ph)
                        .await;
                }
                Some(ChannelCmd::Close) | None => break,
            }
        }
    }
    // Every way out closes, not only the pane's own Close. A pane whose
    // reader is gone leaves a shell still running on the far side, and only
    // a CHANNEL_CLOSE from here gives sshd that session back. After a close
    // the server sent first, both of these put nothing on the wire.
    let _ = channel.eof().await;
    let _ = channel.close().await;
}

/// A session channel for one command, closed whichever way its holder leaves.
///
/// russh sends nothing for a `Channel` that is dropped. Its one close-on-drop
/// sits behind `into_stream`, and a command's output is read with `wait`, not
/// through a stream. Left to the holder, a `?` after the open or the timeout
/// wrapped around the whole exchange leaves the channel open on the server
/// for as long as the connection lives — and sshd counts every one of those
/// against `MaxSessions`, refusing the eleventh.
pub struct CommandChannel {
    channel: Option<Channel<Msg>>,
    runtime: tokio::runtime::Handle,
}

impl CommandChannel {
    fn new(channel: Channel<Msg>) -> Self {
        Self {
            channel: Some(channel),
            // Taken here, on the runtime by construction, rather than looked
            // up in `drop`, which runs wherever the holder is let go.
            runtime: tokio::runtime::Handle::current(),
        }
    }
}

impl std::ops::Deref for CommandChannel {
    type Target = Channel<Msg>;

    fn deref(&self) -> &Channel<Msg> {
        self.channel.as_ref().expect("held until drop")
    }
}

impl std::ops::DerefMut for CommandChannel {
    fn deref_mut(&mut self) -> &mut Channel<Msg> {
        self.channel.as_mut().expect("held until drop")
    }
}

impl Drop for CommandChannel {
    fn drop(&mut self) {
        // `close` only queues the CHANNEL_CLOSE for the session task, which
        // outlives this channel and writes it — the same best effort russh's
        // own close-on-drop makes. A channel the server closed first is
        // already out of the session's table, so this puts nothing more on
        // the wire for it.
        if let Some(channel) = self.channel.take() {
            self.runtime.spawn(async move {
                let _ = channel.close().await;
            });
        }
    }
}

/// sshd's answer to a session open once the connection's `MaxSessions` are
/// all taken, by channels still running or by ones never closed. It says
/// nothing about this channel and everything about the connection — which is
/// full, not broken: every session already on it keeps working.
fn refuses_every_session(e: &russh::Error) -> bool {
    matches!(
        e,
        russh::Error::ChannelOpenFailure(russh::ChannelOpenFailure::ConnectFailed)
    )
}

pub struct SshConnection {
    /// Not behind a lock. Every request on it takes `&self` and waits on a
    /// reply of its own, and a lock held across that wait — a full network
    /// round trip — made every pane on the connection open its channel after
    /// the one before it: a workspace of twenty tabs on a 300ms link took ten
    /// seconds to come back, one tab at a time.
    handle: russh::client::Handle<super::handler::ClientHandler>,
    #[allow(dead_code)]
    key: ConnectionKey,
    remote_forwards: RemoteForwardTable,
    alive: AtomicBool,
    remote_entry: tokio::sync::Mutex<Option<RemoteEntry>>,
    /// What this connection's server probe proved, once it has. See
    /// [`SshConnection::proved_server`].
    proved_server: Mutex<Option<ProvedServer>>,
    /// When the server last refused a session here for being at its limit.
    /// See [`SshConnection::is_saturated`].
    saturated_at: Mutex<Option<Instant>>,
}

impl SshConnection {
    pub(super) fn new(
        handle: russh::client::Handle<super::handler::ClientHandler>,
        key: ConnectionKey,
        remote_forwards: RemoteForwardTable,
    ) -> Arc<Self> {
        Arc::new(Self {
            handle,
            key,
            remote_forwards,
            alive: AtomicBool::new(true),
            remote_entry: tokio::sync::Mutex::new(None),
            proved_server: Mutex::new(None),
            saturated_at: Mutex::new(None),
        })
    }

    #[allow(dead_code)]
    pub fn key(&self) -> &ConnectionKey {
        &self.key
    }

    pub fn is_alive(&self) -> bool {
        if !self.alive.load(Ordering::SeqCst) {
            return false;
        }
        !self.handle.is_closed()
    }

    pub(super) fn mark_dead(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }

    /// Whether the server recently refused a session here because every one
    /// it allows on this connection is taken (sshd's `MaxSessions`, ten by
    /// default — one per remote pane).
    ///
    /// A full connection is not a dead one. Retiring it used to be the answer,
    /// and it took the connection out of the cache while every pane on it was
    /// still using it: the next pane dialled a fresh link and paid a whole
    /// handshake and server probe for it, and so did everything short-lived
    /// after that, because nothing held the replacement. The cache passes a
    /// full connection over instead and adds another beside it.
    pub fn is_saturated(&self) -> bool {
        self.saturated_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some_and(|at| at.elapsed() < SATURATION_HOLD)
    }

    fn set_saturated(&self, saturated: bool) {
        *self
            .saturated_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = saturated.then(Instant::now);
    }

    pub async fn open_session_channel(&self) -> Result<Channel<Msg>, russh::Error> {
        let opened = self.handle.channel_open_session().await;
        match &opened {
            Ok(_) => self.set_saturated(false),
            Err(e) if refuses_every_session(e) => self.set_saturated(true),
            Err(_) => {}
        }
        opened
    }

    /// A session channel for one command, closed whichever way the caller
    /// leaves it — see [`CommandChannel`].
    pub async fn open_command_channel(&self) -> Result<CommandChannel, russh::Error> {
        self.open_session_channel().await.map(CommandChannel::new)
    }

    pub async fn open_direct_tcpip(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Channel<Msg>, russh::Error> {
        self.handle
            .channel_open_direct_tcpip(
                host.to_string(),
                u32::from(port),
                "127.0.0.1".to_string(),
                0,
            )
            .await
    }

    pub async fn open_direct_streamlocal(
        &self,
        socket_path: &str,
    ) -> Result<Channel<Msg>, russh::Error> {
        self.handle
            .channel_open_direct_streamlocal(socket_path.to_string())
            .await
    }

    pub async fn remote_entry_or_init<F, Fut>(&self, init: F) -> RemoteEntry
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = RemoteEntry>,
    {
        let mut guard = self.remote_entry.lock().await;
        if let Some(entry) = guard.as_ref() {
            return entry.clone();
        }
        let entry = init().await;
        log::debug!(
            "ssh {:?}: remote workspace entry is {}",
            self.key,
            entry.kind_label()
        );
        *guard = Some(entry.clone());
        entry
    }

    pub async fn set_remote_entry(&self, entry: RemoteEntry) {
        *self.remote_entry.lock().await = Some(entry);
    }

    /// Where this connection's server was proved to be, and the lock that
    /// makes the second pane wait for the first rather than prove it again.
    ///
    /// The memo lives on the connection rather than beside its key, and that
    /// is the whole of the invalidation story for a reconnect: a dropped or
    /// evicted link is a dropped `SshConnection`, and the one dialled in its
    /// place starts with an empty slot. Nothing has to remember to forget.
    /// What does have to remember is anything that changes the server *over
    /// there* while the link stays up — see
    /// [`crate::daemon::install::forget_remote_server`].
    ///
    /// A blocking `Mutex` on purpose: the probe behind it is a chain of
    /// blocking SSH round trips run on a blocking thread, and a pane that
    /// arrives mid-install wants to wait for that install rather than start a
    /// second one.
    pub(crate) fn proved_server(&self) -> std::sync::MutexGuard<'_, Option<ProvedServer>> {
        self.proved_server
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Where this connection's server was last proved to be, or `None` if the
    /// next pane would have to go and ask — including while it is being asked,
    /// since this never waits. A hint for callers deciding whether a failure is
    /// worth re-proving; the answer itself comes from `ensure_remote_server`.
    pub fn remembered_server(&self) -> Option<String> {
        self.proved_server
            .try_lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(|proved| proved.binary.clone()))
    }

    pub async fn add_remote_forward(
        &self,
        bind_host: &str,
        bind_port: u16,
        target_host: &str,
        target_port: u16,
    ) -> Result<u16, String> {
        if !self
            .remote_forwards
            .register(bind_host, bind_port, target_host, target_port)
        {
            return Err(format!(
                "remote forward {bind_host}:{bind_port} already exists on this connection"
            ));
        }
        let requested = self
            .handle
            .tcpip_forward(bind_host.to_string(), u32::from(bind_port))
            .await;
        match requested {
            Ok(assigned) => {
                let real = if bind_port == 0 {
                    assigned as u16
                } else {
                    bind_port
                };
                if real != bind_port {
                    self.remote_forwards.rekey(bind_host, bind_port, real);
                }
                Ok(real)
            }
            Err(e) => {
                self.remote_forwards.unregister(bind_host, bind_port);
                Err(format!("{e}"))
            }
        }
    }

    pub async fn cancel_remote_forward(&self, bind_host: &str, bind_port: u16) {
        self.remote_forwards.unregister(bind_host, bind_port);
        let _ = self
            .handle
            .cancel_tcpip_forward(bind_host.to_string(), u32::from(bind_port))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{Exec, FakeSshd};
    use super::*;
    use std::io::Read;

    #[test]
    fn reader_delivers_chunks_then_eofs_on_sender_drop() {
        let mut bridge = make_bridge();
        bridge.data_tx.try_send(b"hello ".to_vec()).unwrap();
        bridge.data_tx.try_send(b"world".to_vec()).unwrap();

        let mut buf = [0u8; 64];
        let n = bridge.reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello ");
        let n = bridge.reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"world");

        drop(bridge.data_tx);
        assert_eq!(bridge.reader.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn reader_preserves_chunk_tail_across_reads() {
        let mut bridge = make_bridge();
        bridge.data_tx.try_send(b"abcdef".to_vec()).unwrap();
        let mut small = [0u8; 4];
        let n = bridge.reader.read(&mut small).unwrap();
        assert_eq!(&small[..n], b"abcd");
        let n = bridge.reader.read(&mut small).unwrap();
        assert_eq!(&small[..n], b"ef");
    }

    #[test]
    fn bounded_channel_applies_backpressure_until_drained() {
        let mut bridge = make_bridge();
        for i in 0..DATA_CHANNEL_DEPTH {
            bridge
                .data_tx
                .try_send(vec![i as u8])
                .expect("within capacity");
        }
        assert!(bridge.data_tx.try_send(vec![0xff]).is_err());

        let mut buf = [0u8; 8];
        let n = bridge.reader.read(&mut buf).unwrap();
        assert_eq!(n, 1);
        assert!(bridge.data_tx.try_send(vec![0xff]).is_ok());
    }

    /// The pane's reader is gone before the shell's first byte, and the
    /// shell keeps running: the one exit from `drive_channel` that used to
    /// drop the channel with the far side none the wiser.
    #[tokio::test]
    async fn a_pane_that_is_gone_closes_the_shell_channel_behind_it() {
        let sshd = FakeSshd::connect(Exec::Hangs, None).await;
        let channel = sshd
            .conn
            .open_session_channel()
            .await
            .expect("open the shell channel");
        channel.request_shell(true).await.expect("shell request");

        let BridgeEnds {
            reader,
            writer: _writer,
            handle: _handle,
            data_tx,
            cmd_rx,
        } = make_bridge();
        drop(reader);

        drive_channel(channel, data_tx, cmd_rx, sshd.conn.clone()).await;
        sshd.wait_for_closed(1).await;
        assert_eq!(sshd.opened(), 1);
    }

    #[tokio::test]
    async fn a_link_at_its_session_limit_is_full_not_dead() {
        let sshd = FakeSshd::connect(Exec::Hangs, Some(1)).await;
        let held = sshd
            .conn
            .open_session_channel()
            .await
            .expect("the one session the server allows");
        assert!(sshd.conn.is_alive());

        let refused = sshd.conn.open_session_channel().await;
        assert!(
            matches!(
                refused,
                Err(russh::Error::ChannelOpenFailure(
                    russh::ChannelOpenFailure::ConnectFailed
                ))
            ),
            "the fake answers like sshd at MaxSessions: {refused:?}"
        );
        assert!(
            sshd.conn.is_saturated(),
            "the cache must pass this link over for the next session"
        );
        assert!(
            sshd.conn.is_alive(),
            "the session already on it is still in use"
        );
        assert_eq!(sshd.refused(), 1);

        held.close().await.expect("close the held session");
        sshd.wait_for_closed(1).await;
        let _again = sshd
            .conn
            .open_session_channel()
            .await
            .expect("a slot freed up");
        assert!(
            !sshd.conn.is_saturated(),
            "a session that opens says the link has room again"
        );
    }

    #[test]
    fn only_a_refused_connect_means_the_link_is_spent() {
        use russh::ChannelOpenFailure;
        assert!(refuses_every_session(&russh::Error::ChannelOpenFailure(
            ChannelOpenFailure::ConnectFailed
        )));
        // A user whose account may not open sessions gets this on a fresh
        // link too; retiring it would only dial again to be told the same.
        assert!(!refuses_every_session(&russh::Error::ChannelOpenFailure(
            ChannelOpenFailure::AdministrativelyProhibited
        )));
        assert!(!refuses_every_session(&russh::Error::Disconnect));
    }
}

impl Drop for SshConnection {
    fn drop(&mut self) {
        self.mark_dead();
    }
}

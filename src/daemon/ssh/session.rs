//! The async↔blocking bridge for a native-SSH pane, plus the connection wrapper.
//!
//! The daemon's pane reader/writer threads are plain std threads doing *blocking*
//! `Read`/`Write` (see `daemon::pane`). russh is async. This module is the seam:
//!
//! - [`SshReader`] is a blocking `Read` over a **bounded** channel fed by the
//!   channel driver. A full channel makes the driver's `data_tx.send().await`
//!   pause, which stops it draining `channel.wait()`, which lets russh's own
//!   window management apply backpressure to the SSH channel — so a slow client
//!   (via `OutputGate`) throttles the remote exactly like a full PTY throttles a
//!   local child, with no unbounded spool in between. Channel EOF/close drops
//!   `data_tx`, so `blocking_recv()` returns `None` and the read returns `Ok(0)`
//!   — the same liveness signal a PTY hangup gives, feeding the existing death
//!   path.
//! - [`SshWriter`] is a blocking `Write` that forwards bytes to the driver over an
//!   unbounded command channel (keystrokes are low-volume; never block input).
//! - [`ChannelCmd`] carries input / resize / close from the pane's std threads to
//!   the async driver.
//! - [`drive_channel`] is the per-pane async task pumping the shell channel.
//! - [`SshConnection`] wraps one authenticated `russh::client::Handle`. It is the
//!   unit of reuse and of the FR-C2 blast radius (see the doc comment there), and
//!   the API surface WS4 (port-forwards) and WS5 (SFTP) reuse to open further
//!   channels on a pane's existing connection.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use russh::client::Msg;
use russh::{Channel, ChannelMsg};

use crate::daemon::protocol::WinSize;

use super::ConnectionKey;
use super::forward::RemoteForwardTable;

/// Bounded depth (in messages) of the driver→reader data channel. Each message is
/// one russh data chunk (≤ the channel's max packet size, ~32 KiB), so this caps
/// the in-flight spool at a few hundred KiB before backpressure engages — small
/// enough to keep memory bounded, large enough not to stall a healthy client.
const DATA_CHANNEL_DEPTH: usize = 16;

/// A slot the connect task publishes the pane's established connection into, as a
/// `Weak` so it never keeps the connection alive past the shell's own strong
/// `Arc` (held by the channel driver). WS4 (forwards) and WS5 (SFTP) upgrade it —
/// via `DaemonPane::ssh_connection()` — to open further channels on the pane's
/// shared connection. Empty until the connection authenticates.
pub type SharedConnection = Arc<Mutex<Weak<SshConnection>>>;

/// A command from the pane's std threads to the async channel driver.
pub enum ChannelCmd {
    /// Bytes to write to the shell channel (keyboard input / paste / login script).
    Data(Vec<u8>),
    /// A terminal resize → `window-change` request.
    Resize(WinSize),
    /// Close the channel (kill/hangup). The driver then exits and its EOF reaches
    /// the reader.
    Close,
}

/// The pane-facing handle for a native-SSH session: where resize/close/input
/// commands are sent. Cloned into the [`SshWriter`] and held by the pane's
/// backend so `resize`/`kill` reach the driver.
pub struct SshSessionHandle {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<ChannelCmd>,
}

impl SshSessionHandle {
    pub fn resize(&self, size: WinSize) {
        // A closed channel just means the driver already exited (the pane is
        // dying); dropping the resize is correct.
        let _ = self.cmd_tx.send(ChannelCmd::Resize(size));
    }

    /// Ask the driver to close the shell channel. Idempotent — a second send after
    /// the driver exited is a harmless no-op.
    pub fn close(&self) {
        let _ = self.cmd_tx.send(ChannelCmd::Close);
    }

    fn send_data(&self, bytes: Vec<u8>) -> io::Result<()> {
        self.cmd_tx
            .send(ChannelCmd::Data(bytes))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "ssh channel closed"))
    }
}

/// Blocking `Read` half of the bridge — see the module comment.
pub struct SshReader {
    rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    leftover: Vec<u8>,
    pos: usize,
}

impl Read for SshReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Drain any partial chunk left from a previous read first.
        while self.pos >= self.leftover.len() {
            match self.rx.blocking_recv() {
                Some(data) if !data.is_empty() => {
                    self.leftover = data;
                    self.pos = 0;
                }
                // An empty chunk shouldn't occur (the driver only forwards
                // non-empty data), but if it did, just wait for the next.
                Some(_) => continue,
                // Sender dropped: channel EOF/close. Report clean EOF so the
                // pane's death path fires exactly as on a PTY hangup.
                None => return Ok(0),
            }
        }
        let n = (self.leftover.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.leftover[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Blocking `Write` half of the bridge — forwards input bytes to the driver.
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

/// Build the paired ends of the bridge for one pane: the blocking reader/writer
/// the daemon threads use, the shared handle for resize/close, and the two channel
/// ends the async driver takes (`data_tx` to push output, `cmd_rx` to pull input).
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

/// Pixel dims for a `window-change`, mirroring `pty_size` in `daemon::pane`.
fn pixels(size: WinSize) -> (u32, u32) {
    (
        u32::from(size.cols).saturating_mul(u32::from(size.cell_w)),
        u32::from(size.rows).saturating_mul(u32::from(size.cell_h)),
    )
}

/// The per-pane async task: pump shell-channel output to the reader and channel
/// commands to the remote. Ends (dropping `data_tx`, EOFing the reader) on channel
/// EOF/close, an explicit `Close`, or the command sender being dropped (pane
/// gone). `_conn` is held for the session's lifetime so the shared connection
/// isn't dropped (and disconnected) while this shell is still open.
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
                    // Awaiting here is the backpressure point: a full bounded
                    // channel pauses us, which pauses `channel.wait()`, which lets
                    // russh throttle the SSH window. An error means the reader was
                    // dropped (pane gone) — stop.
                    if data_tx.send(data.to_vec()).await.is_err() {
                        break;
                    }
                }
                // Merge stderr (extended data) into the same byte stream: a shell
                // channel's stderr is part of the terminal output the user expects
                // to see inline, exactly as a PTY interleaves them.
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    if data_tx.send(data.to_vec()).await.is_err() {
                        break;
                    }
                }
                // Exit status/signal arrive before the final Eof/Close; record
                // nothing special — the daemon's existing `Exited{code:None}` path
                // (driven by the reader's EOF below) is what the GUI consumes, and
                // it doesn't depend on the code. Keep looping for any trailing data.
                Some(ChannelMsg::ExitStatus { .. }) | Some(ChannelMsg::ExitSignal { .. }) => {}
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(ChannelCmd::Data(bytes)) => {
                    // `&[u8]` implements tokio's AsyncRead; this writes one data
                    // message. A failure means the channel is gone — let the
                    // wait() side observe the close.
                    let _ = channel.data(&bytes[..]).await;
                }
                Some(ChannelCmd::Resize(size)) => {
                    let (pw, ph) = pixels(size);
                    let _ = channel
                        .window_change(u32::from(size.cols), u32::from(size.rows), pw, ph)
                        .await;
                }
                // Explicit close, or the pane dropped its command sender: tear the
                // channel down and exit so the reader EOFs.
                Some(ChannelCmd::Close) | None => {
                    let _ = channel.eof().await;
                    let _ = channel.close().await;
                    break;
                }
            }
        }
    }
    // Falling out of the loop drops `data_tx`; the reader's next `blocking_recv`
    // returns `None` → `read` returns `Ok(0)` → the pane reports `Exited`.
}

/// One authenticated russh connection, shared by every pane (and later every SFTP
/// session / port-forward) that resolved to the same [`ConnectionKey`].
///
/// **Blast radius (FR-C2).** All shell channels for a given key share this one
/// `Handle`. If the underlying transport drops, every channel opened on it EOFs
/// at once, so *every* pane sharing the connection sees `Exited` together — the
/// PRD's documented "all shared panes go disconnected as a unit" semantics. The
/// connection stays in the registry as a `Weak`; when the last shell/SFTP/forward
/// that holds an `Arc<SshConnection>` drops, this `Drop` disconnects the session.
/// A subsequent spawn for the same key finds either a live connection (reuse, no
/// re-auth — new tabs are instant) or a dead/absent one (a fresh connect).
pub struct SshConnection {
    /// The authenticated russh handle. `russh::client::Handle` is `Send` but not
    /// `Sync` (it owns an `UnboundedReceiver`), yet the connection registry and the
    /// static [`super::SshManager`] must be `Sync`. A `tokio::Mutex` makes the
    /// whole `SshConnection` `Send + Sync`; the lock is uncontended (channel opens
    /// are infrequent) and holding it across the open `.await` is exactly what
    /// tokio mutexes are for.
    handle: tokio::sync::Mutex<russh::client::Handle<super::handler::ClientHandler>>,
    /// The key this connection is registered under. Retained for diagnostics and
    /// as the stable identity WS4/WS5 will match against.
    #[allow(dead_code)]
    key: ConnectionKey,
    /// The connection's active `tcpip-forward` bindings (WS4 Remote forwards).
    /// Shared with this connection's [`super::handler::ClientHandler`] so incoming
    /// `forwarded-tcpip` channels resolve to a local target. Empty for a connection
    /// with no remote forwards.
    remote_forwards: RemoteForwardTable,
    alive: AtomicBool,
}

impl SshConnection {
    pub(super) fn new(
        handle: russh::client::Handle<super::handler::ClientHandler>,
        key: ConnectionKey,
        remote_forwards: RemoteForwardTable,
    ) -> Arc<Self> {
        Arc::new(Self {
            handle: tokio::sync::Mutex::new(handle),
            key,
            remote_forwards,
            alive: AtomicBool::new(true),
        })
    }

    #[allow(dead_code)] // WS4/WS5 seam: identify a pane's shared connection
    pub fn key(&self) -> &ConnectionKey {
        &self.key
    }

    /// Whether this connection is still usable for reuse.
    ///
    /// Two signals: the `alive` flag (cleared by [`mark_dead`](Self::mark_dead) on
    /// teardown or when a reuse attempt finds the transport dead) **and** the russh
    /// handle's own liveness — when russh's session task ends (transport dropped),
    /// its command sender closes, so `handle.is_closed()` flips to true. The flag
    /// alone is unreliable: `mark_dead` only runs from `Drop`, but a parked
    /// forward/loopback accept loop holds an `Arc<SshConnection>`, so a dead
    /// connection's `Drop` never runs and the flag stays true. Consulting
    /// `is_closed()` (via a non-blocking `try_lock`; a contended lock means an open
    /// is in flight, so assume alive) catches that case cheaply. The self-healing
    /// reconnect in `SshManager::run_session` is the belt-and-suspenders backstop.
    pub fn is_alive(&self) -> bool {
        if !self.alive.load(Ordering::SeqCst) {
            return false;
        }
        match self.handle.try_lock() {
            Ok(handle) => !handle.is_closed(),
            Err(_) => true,
        }
    }

    /// Mark this connection unusable for reuse (teardown, or a reuse attempt that
    /// found the transport dead). Idempotent.
    pub(super) fn mark_dead(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }

    /// Open a new interactive session channel on this connection. Used for shells
    /// (WS2) and reused by WS5 to open the SFTP subsystem channel on a pane's
    /// existing connection.
    pub async fn open_session_channel(&self) -> Result<Channel<Msg>, russh::Error> {
        self.handle.lock().await.channel_open_session().await
    }

    /// Open a `direct-tcpip` channel to `host:port` through this connection. This
    /// is both the jump-host transport primitive (WS2) and the Local/Dynamic
    /// port-forward primitive WS4 will build on, opened on the pane's shared
    /// connection rather than a control socket.
    pub async fn open_direct_tcpip(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Channel<Msg>, russh::Error> {
        self.handle
            .lock()
            .await
            .channel_open_direct_tcpip(
                host.to_string(),
                u32::from(port),
                "127.0.0.1".to_string(),
                0,
            )
            .await
    }

    /// Request a `tcpip-forward` binding on `bind_host:bind_port`, routing incoming
    /// `forwarded-tcpip` channels to `target_host:target_port` (WS4 Remote forward).
    /// Registers the target *before* the request so an eager server channel finds
    /// it. Returns the resolved bind port (the server assigns one when `bind_port`
    /// is 0). On failure the registration is rolled back.
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
            .lock()
            .await
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

    /// Cancel a previously requested `tcpip-forward` binding (best effort) and drop
    /// its target registration.
    pub async fn cancel_remote_forward(&self, bind_host: &str, bind_port: u16) {
        self.remote_forwards.unregister(bind_host, bind_port);
        let _ = self
            .handle
            .lock()
            .await
            .cancel_tcpip_forward(bind_host.to_string(), u32::from(bind_port))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// The blocking reader delivers pushed chunks in order and, once the driver
    /// drops its `data_tx`, reports clean EOF (`Ok(0)`) — the liveness signal the
    /// pane's death path keys off, identical to a PTY hangup.
    #[test]
    fn reader_delivers_chunks_then_eofs_on_sender_drop() {
        let mut bridge = make_bridge();
        // Push two chunks into the driver→reader channel (buffered; capacity 16).
        bridge.data_tx.try_send(b"hello ".to_vec()).unwrap();
        bridge.data_tx.try_send(b"world".to_vec()).unwrap();

        let mut buf = [0u8; 64];
        let n = bridge.reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello ");
        let n = bridge.reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"world");

        // Drop the sender: the next read must EOF, not block forever.
        drop(bridge.data_tx);
        assert_eq!(bridge.reader.read(&mut buf).unwrap(), 0);
    }

    /// A partial read keeps the chunk's tail buffered for the next read (the reader
    /// must never drop bytes when `buf` is smaller than a chunk).
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

    /// The bounded data channel applies backpressure: once `DATA_CHANNEL_DEPTH`
    /// chunks are queued, a further push is refused (`Full`) until the reader
    /// drains one — this is what makes a slow client (via `OutputGate`) throttle
    /// the SSH channel window instead of spooling unboundedly.
    #[test]
    fn bounded_channel_applies_backpressure_until_drained() {
        let mut bridge = make_bridge();
        for i in 0..DATA_CHANNEL_DEPTH {
            bridge
                .data_tx
                .try_send(vec![i as u8])
                .expect("within capacity");
        }
        // At capacity: a further push is rejected rather than buffered.
        assert!(bridge.data_tx.try_send(vec![0xff]).is_err());

        // Drain one chunk; capacity frees, so the next push succeeds.
        let mut buf = [0u8; 8];
        let n = bridge.reader.read(&mut buf).unwrap();
        assert_eq!(n, 1);
        assert!(bridge.data_tx.try_send(vec![0xff]).is_ok());
    }
}

impl Drop for SshConnection {
    fn drop(&mut self) {
        // The last holder of the connection is going away. Marking dead keeps a
        // racing reuse from adopting it. Dropping `self.handle` (which happens
        // right after this) drops the last sender to russh's session task, which
        // ends the session and closes the transport — an immediate teardown. A
        // *clean* protocol disconnect would need an `.await`, which `Drop` can't
        // do; an abrupt close is the right behavior for teardown anyway.
        self.mark_dead();
    }
}

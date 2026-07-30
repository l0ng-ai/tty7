use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use russh::client::Msg;
use russh::{Channel, ChannelMsg};

use crate::daemon::protocol::WinSize;
use crate::daemon::remote_link::RemoteEntry;

use super::ConnectionKey;
use super::forward::RemoteForwardTable;

const DATA_CHANNEL_DEPTH: usize = 16;

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
                Some(ChannelCmd::Close) | None => {
                    let _ = channel.eof().await;
                    let _ = channel.close().await;
                    break;
                }
            }
        }
    }
}

pub struct SshConnection {
    handle: tokio::sync::Mutex<russh::client::Handle<super::handler::ClientHandler>>,
    #[allow(dead_code)]
    key: ConnectionKey,
    remote_forwards: RemoteForwardTable,
    alive: AtomicBool,
    remote_entry: tokio::sync::Mutex<Option<RemoteEntry>>,
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
            remote_entry: tokio::sync::Mutex::new(None),
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
        match self.handle.try_lock() {
            Ok(handle) => !handle.is_closed(),
            Err(_) => true,
        }
    }

    pub(super) fn mark_dead(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }

    pub async fn open_session_channel(&self) -> Result<Channel<Msg>, russh::Error> {
        self.handle.lock().await.channel_open_session().await
    }

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

    pub async fn open_direct_streamlocal(
        &self,
        socket_path: &str,
    ) -> Result<Channel<Msg>, russh::Error> {
        self.handle
            .lock()
            .await
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
}

impl Drop for SshConnection {
    fn drop(&mut self) {
        self.mark_dead();
    }
}

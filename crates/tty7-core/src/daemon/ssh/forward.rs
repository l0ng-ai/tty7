use std::collections::HashMap;
use std::io;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::core::session::WorkspaceId;
use crate::daemon::protocol::{
    ForwardStatus, LoopbackForward, ManagedForward, NativeSshSpec, SshForwardKind, SshForwardRule,
};

use super::session::SshConnection;
use super::{ConnectionKey, SshManager};

async fn accept_retrying(listener: &TcpListener) -> Option<(TcpStream, std::net::SocketAddr)> {
    let mut failures = 0u32;
    loop {
        match listener.accept().await {
            Ok(pair) => return Some(pair),
            Err(_) if failures >= 10 => return None,
            Err(_) => {
                failures += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50 << failures.min(5))).await;
            }
        }
    }
}

pub(super) async fn bridge<A, B>(a: A, b: B) -> io::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut ar, mut aw) = tokio::io::split(a);
    let (mut br, mut bw) = tokio::io::split(b);

    let a_to_b = async {
        tokio::io::copy(&mut ar, &mut bw).await?;
        bw.shutdown().await
    };
    let b_to_a = async {
        tokio::io::copy(&mut br, &mut aw).await?;
        aw.shutdown().await
    };

    tokio::try_join!(a_to_b, b_to_a)?;
    Ok(())
}

pub(super) async fn socks5_negotiate<S>(s: &mut S) -> io::Result<(String, u16)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut head = [0u8; 2];
    s.read_exact(&mut head).await?;
    if head[0] != 0x05 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported SOCKS version (only SOCKS5 is accepted)",
        ));
    }
    let nmethods = head[1] as usize;
    let mut methods = vec![0u8; nmethods];
    s.read_exact(&mut methods).await?;
    if !methods.contains(&0x00) {
        let _ = s.write_all(&[0x05, 0xFF]).await;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SOCKS5 client offered no no-auth method",
        ));
    }
    s.write_all(&[0x05, 0x00]).await?;

    let mut req = [0u8; 4];
    s.read_exact(&mut req).await?;
    if req[0] != 0x05 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SOCKS5 request had wrong version",
        ));
    }
    if req[1] != 0x01 {
        socks5_reply(s, 0x07).await?;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SOCKS5 command not supported (only CONNECT)",
        ));
    }
    let host = match req[3] {
        0x01 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await?;
            Ipv4Addr::from(a).to_string()
        }
        0x04 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await?;
            std::net::Ipv6Addr::from(a).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            s.read_exact(&mut name).await?;
            String::from_utf8(name).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "SOCKS5 domain not UTF-8")
            })?
        }
        other => {
            socks5_reply(s, 0x08).await?;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("SOCKS5 unsupported address type {other}"),
            ));
        }
    };
    let mut port = [0u8; 2];
    s.read_exact(&mut port).await?;
    Ok((host, u16::from_be_bytes(port)))
}

pub(super) async fn socks5_reply<S>(s: &mut S, rep: u8) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    s.write_all(&[0x05, rep, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await
}

#[derive(Clone, Default)]
pub struct RemoteForwardTable {
    inner: Arc<Mutex<HashMap<(String, u16), (String, u16)>>>,
}

impl RemoteForwardTable {
    pub(super) fn register(
        &self,
        bind_host: &str,
        bind_port: u16,
        target_host: &str,
        target_port: u16,
    ) -> bool {
        match self
            .inner
            .lock()
            .unwrap()
            .entry((bind_host.to_string(), bind_port))
        {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((target_host.to_string(), target_port));
                true
            }
        }
    }

    pub(super) fn unregister(&self, bind_host: &str, bind_port: u16) {
        self.inner
            .lock()
            .unwrap()
            .remove(&(bind_host.to_string(), bind_port));
    }

    pub(super) fn rekey(&self, bind_host: &str, from_port: u16, to_port: u16) {
        let mut map = self.inner.lock().unwrap();
        if let Some(target) = map.remove(&(bind_host.to_string(), from_port)) {
            map.insert((bind_host.to_string(), to_port), target);
        }
    }

    pub(super) fn lookup(
        &self,
        connected_address: &str,
        connected_port: u16,
    ) -> Option<(String, u16)> {
        let map = self.inner.lock().unwrap();
        if let Some(t) = map.get(&(connected_address.to_string(), connected_port)) {
            return Some(t.clone());
        }
        let mut same_port = map.iter().filter(|((_, p), _)| *p == connected_port);
        match (same_port.next(), same_port.next()) {
            (Some((_, t)), None) => Some(t.clone()),
            _ => None,
        }
    }
}

enum ForwardCancel {
    Task(JoinHandle<()>),
    Remote {
        conn: Weak<SshConnection>,
        bind_host: String,
        bind_port: u16,
    },
    None,
}

struct ForwardEntry {
    id: u64,
    kind: SshForwardKind,
    bind_host: String,
    bind_port: u16,
    target_host: String,
    target_port: u16,
    description: Option<String>,
    status: ForwardStatus,
    cancel: ForwardCancel,
    auto_local: bool,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ForwardOwner {
    Pane(u64),
    Workspace(WorkspaceId),
}

impl ForwardEntry {
    fn to_managed(&self, pane_id: u64) -> ManagedForward {
        ManagedForward {
            id: self.id,
            pane_id,
            kind: self.kind,
            bind_host: self.bind_host.clone(),
            bind_port: self.bind_port,
            target_host: self.target_host.clone(),
            target_port: self.target_port,
            description: self.description.clone(),
            status: self.status.clone(),
        }
    }
}

#[derive(Default)]
pub struct SshForwardRegistry {
    owners: Mutex<HashMap<ForwardOwner, Vec<ForwardEntry>>>,
    next_id: AtomicU64,
}

impl SshForwardRegistry {
    pub async fn establish(
        &self,
        pane_id: u64,
        conn: Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> ManagedForward {
        self.establish_owned(&ForwardOwner::Pane(pane_id), pane_id, conn, rule)
            .await
    }

    pub fn list(&self, pane_id: u64) -> Vec<ManagedForward> {
        self.list_owned(&ForwardOwner::Pane(pane_id), pane_id)
    }

    pub async fn remove(&self, pane_id: u64, forward_id: u64) -> Vec<ManagedForward> {
        self.remove_owned(&ForwardOwner::Pane(pane_id), pane_id, forward_id)
            .await
    }

    pub async fn teardown_pane(&self, pane_id: u64) {
        self.teardown_owned(&ForwardOwner::Pane(pane_id)).await;
    }

    pub async fn establish_workspace(
        &self,
        workspace: WorkspaceId,
        view_pane: u64,
        conn: Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> ManagedForward {
        self.establish_owned(&ForwardOwner::Workspace(workspace), view_pane, conn, rule)
            .await
    }

    pub fn list_workspace(&self, workspace: WorkspaceId, view_pane: u64) -> Vec<ManagedForward> {
        self.list_owned(&ForwardOwner::Workspace(workspace), view_pane)
    }

    pub async fn remove_workspace(
        &self,
        workspace: WorkspaceId,
        view_pane: u64,
        forward_id: u64,
    ) -> Vec<ManagedForward> {
        self.remove_owned(&ForwardOwner::Workspace(workspace), view_pane, forward_id)
            .await
    }

    pub async fn teardown_workspace(&self, workspace: WorkspaceId) {
        self.teardown_owned(&ForwardOwner::Workspace(workspace))
            .await;
    }

    async fn establish_owned(
        &self,
        owner: &ForwardOwner,
        view_pane: u64,
        conn: Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> ManagedForward {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (bind_port, status, cancel) = match rule.kind {
            SshForwardKind::Local => self.start_local(&conn, rule).await,
            SshForwardKind::Dynamic => self.start_dynamic(&conn, rule).await,
            SshForwardKind::Remote => self.start_remote(&conn, rule).await,
        };
        let entry = ForwardEntry {
            id,
            kind: rule.kind,
            bind_host: rule.bind_host.clone(),
            bind_port,
            target_host: rule.target_host.clone(),
            target_port: rule.target_port,
            description: rule.description.clone(),
            status,
            cancel,
            auto_local: false,
        };
        let managed = entry.to_managed(view_pane);
        self.owners
            .lock()
            .unwrap()
            .entry(owner.clone())
            .or_default()
            .push(entry);
        managed
    }

    fn list_owned(&self, owner: &ForwardOwner, view_pane: u64) -> Vec<ManagedForward> {
        let owners = self.owners.lock().unwrap();
        let mut list: Vec<_> = owners
            .get(owner)
            .into_iter()
            .flatten()
            .map(|e| e.to_managed(view_pane))
            .collect();
        list.sort_by_key(|m| m.id);
        list
    }

    async fn remove_owned(
        &self,
        owner: &ForwardOwner,
        view_pane: u64,
        forward_id: u64,
    ) -> Vec<ManagedForward> {
        let removed = {
            let mut owners = self.owners.lock().unwrap();
            owners.get_mut(owner).and_then(|entries| {
                let pos = entries.iter().position(|e| e.id == forward_id)?;
                Some(entries.remove(pos))
            })
        };
        if let Some(entry) = removed {
            Self::cancel_entry(entry).await;
        }
        self.list_owned(owner, view_pane)
    }

    async fn teardown_owned(&self, owner: &ForwardOwner) {
        let entries = self.owners.lock().unwrap().remove(owner);
        for entry in entries.into_iter().flatten() {
            Self::cancel_entry(entry).await;
        }
    }

    async fn cancel_entry(entry: ForwardEntry) {
        match entry.cancel {
            ForwardCancel::Task(handle) => {
                handle.abort();
                let _ = handle.await;
            }
            ForwardCancel::Remote {
                conn,
                bind_host,
                bind_port,
            } => {
                if let Some(conn) = conn.upgrade() {
                    conn.cancel_remote_forward(&bind_host, bind_port).await;
                }
            }
            ForwardCancel::None => {}
        }
    }

    async fn start_local(
        &self,
        conn: &Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> (u16, ForwardStatus, ForwardCancel) {
        let listener = match TcpListener::bind((rule.bind_host.as_str(), rule.bind_port)).await {
            Ok(l) => l,
            Err(e) => {
                return (
                    rule.bind_port,
                    ForwardStatus::Error(format!(
                        "bind {}:{} failed: {e}",
                        rule.bind_host, rule.bind_port
                    )),
                    ForwardCancel::None,
                );
            }
        };
        let bound = listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(rule.bind_port);
        let conn = conn.clone();
        let target_host = rule.target_host.clone();
        let target_port = rule.target_port;
        let handle = tokio::spawn(async move {
            loop {
                let sock = match accept_retrying(&listener).await {
                    Some((sock, _peer)) => sock,
                    None => break,
                };
                if !conn.is_alive() {
                    break;
                }
                let conn = conn.clone();
                let target_host = target_host.clone();
                tokio::spawn(async move {
                    match conn.open_direct_tcpip(&target_host, target_port).await {
                        Ok(channel) => {
                            let _ = bridge(sock, channel.into_stream()).await;
                        }
                        Err(e) => {
                            log::info!("local forward to {target_host}:{target_port} rejected: {e}")
                        }
                    }
                });
            }
        });
        (bound, ForwardStatus::Listening, ForwardCancel::Task(handle))
    }

    async fn start_dynamic(
        &self,
        conn: &Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> (u16, ForwardStatus, ForwardCancel) {
        let listener = match TcpListener::bind((rule.bind_host.as_str(), rule.bind_port)).await {
            Ok(l) => l,
            Err(e) => {
                return (
                    rule.bind_port,
                    ForwardStatus::Error(format!(
                        "bind {}:{} failed: {e}",
                        rule.bind_host, rule.bind_port
                    )),
                    ForwardCancel::None,
                );
            }
        };
        let bound = listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(rule.bind_port);
        let conn = conn.clone();
        let handle = tokio::spawn(async move {
            loop {
                let sock = match accept_retrying(&listener).await {
                    Some((sock, _peer)) => sock,
                    None => break,
                };
                if !conn.is_alive() {
                    break;
                }
                let conn = conn.clone();
                tokio::spawn(async move {
                    let mut sock = sock;
                    let (host, port) = match socks5_negotiate(&mut sock).await {
                        Ok(t) => t,
                        Err(e) => {
                            log::info!("dynamic forward: SOCKS5 negotiation failed: {e}");
                            return;
                        }
                    };
                    match conn.open_direct_tcpip(&host, port).await {
                        Ok(channel) => {
                            if socks5_reply(&mut sock, 0x00).await.is_err() {
                                return;
                            }
                            let _ = bridge(sock, channel.into_stream()).await;
                        }
                        Err(e) => {
                            let _ = socks5_reply(&mut sock, 0x05).await;
                            log::info!("dynamic forward to {host}:{port} rejected: {e}");
                        }
                    }
                });
            }
        });
        (bound, ForwardStatus::Listening, ForwardCancel::Task(handle))
    }

    async fn start_remote(
        &self,
        conn: &Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> (u16, ForwardStatus, ForwardCancel) {
        match conn
            .add_remote_forward(
                &rule.bind_host,
                rule.bind_port,
                &rule.target_host,
                rule.target_port,
            )
            .await
        {
            Ok(bound) => (
                bound,
                ForwardStatus::Listening,
                ForwardCancel::Remote {
                    conn: Arc::downgrade(conn),
                    bind_host: rule.bind_host.clone(),
                    bind_port: bound,
                },
            ),
            Err(e) => (
                rule.bind_port,
                ForwardStatus::Error(format!("remote forward request denied: {e}")),
                ForwardCancel::None,
            ),
        }
    }

    pub async fn ensure_loopback(
        &self,
        pane_id: u64,
        conn: Arc<SshConnection>,
        _target: &str,
        remote_host: &str,
        remote_port: u16,
    ) -> io::Result<LoopbackForward> {
        self.ensure_loopback_owned(&ForwardOwner::Pane(pane_id), conn, remote_host, remote_port)
            .await
    }

    pub async fn ensure_loopback_workspace(
        &self,
        workspace: WorkspaceId,
        conn: Arc<SshConnection>,
        remote_host: &str,
        remote_port: u16,
    ) -> io::Result<LoopbackForward> {
        self.ensure_loopback_owned(
            &ForwardOwner::Workspace(workspace),
            conn,
            remote_host,
            remote_port,
        )
        .await
    }

    async fn ensure_loopback_owned(
        &self,
        owner: &ForwardOwner,
        conn: Arc<SshConnection>,
        remote_host: &str,
        remote_port: u16,
    ) -> io::Result<LoopbackForward> {
        if let Some(local_port) = self.find_auto_local(owner, remote_host, remote_port) {
            return Ok(LoopbackForward { local_port });
        }
        let rule = SshForwardRule {
            kind: SshForwardKind::Local,
            bind_host: "127.0.0.1".to_string(),
            bind_port: 0,
            target_host: remote_host.to_string(),
            target_port: remote_port,
            description: Some(format!("localhost link → :{remote_port}")),
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (bind_port, status, cancel) = self.start_local(&conn, &rule).await;
        if let ForwardStatus::Error(e) = &status {
            return Err(io::Error::other(e.clone()));
        }
        let entry = ForwardEntry {
            id,
            kind: SshForwardKind::Local,
            bind_host: rule.bind_host.clone(),
            bind_port,
            target_host: rule.target_host.clone(),
            target_port: rule.target_port,
            description: rule.description.clone(),
            status,
            cancel,
            auto_local: true,
        };
        self.owners
            .lock()
            .unwrap()
            .entry(owner.clone())
            .or_default()
            .push(entry);
        Ok(LoopbackForward {
            local_port: bind_port,
        })
    }

    fn find_auto_local(
        &self,
        owner: &ForwardOwner,
        remote_host: &str,
        remote_port: u16,
    ) -> Option<u16> {
        let owners = self.owners.lock().unwrap();
        owners
            .get(owner)?
            .iter()
            .find(|e| {
                e.auto_local
                    && e.kind == SshForwardKind::Local
                    && e.target_host == remote_host
                    && e.target_port == remote_port
                    && matches!(e.status, ForwardStatus::Listening)
            })
            .map(|e| e.bind_port)
    }
}

impl SshManager {
    pub fn existing_connection(&self, spec: &NativeSshSpec) -> Option<Arc<SshConnection>> {
        let key = ConnectionKey::from_spec(spec);
        let slot = self.conns.lock().unwrap().get(&key).cloned()?;
        let guard = slot.try_lock().ok()?;
        let conn = guard.upgrade()?;
        conn.is_alive().then_some(conn)
    }

    pub fn add_workspace_forward(
        &self,
        workspace: WorkspaceId,
        view_pane: u64,
        conn: Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> Vec<ManagedForward> {
        self.runtime.block_on(async {
            self.forwards
                .establish_workspace(workspace, view_pane, conn, rule)
                .await;
            self.forwards.list_workspace(workspace, view_pane)
        })
    }

    pub fn remove_workspace_forward(
        &self,
        workspace: WorkspaceId,
        view_pane: u64,
        forward_id: u64,
    ) -> Vec<ManagedForward> {
        self.runtime.block_on(
            self.forwards
                .remove_workspace(workspace, view_pane, forward_id),
        )
    }

    pub fn list_workspace_forwards(
        &self,
        workspace: WorkspaceId,
        view_pane: u64,
    ) -> Vec<ManagedForward> {
        self.forwards.list_workspace(workspace, view_pane)
    }

    pub fn teardown_workspace_forwards(&self, workspace: WorkspaceId) {
        self.runtime
            .block_on(self.forwards.teardown_workspace(workspace));
    }

    pub fn ensure_workspace_loopback(
        &self,
        workspace: WorkspaceId,
        conn: Arc<SshConnection>,
        remote_host: &str,
        remote_port: u16,
    ) -> io::Result<LoopbackForward> {
        self.runtime
            .block_on(self.forwards.ensure_loopback_workspace(
                workspace,
                conn,
                remote_host,
                remote_port,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn socks5_rejects_v4() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&[0x04, 0x01]).await.unwrap();
        let err = socks5_negotiate(&mut server).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn socks5_v5_connect_ipv4() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        client
            .write_all(&[0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4, 0x00, 0x50])
            .await
            .unwrap();
        let (host, port) = socks5_negotiate(&mut server).await.unwrap();
        assert_eq!(host, "1.2.3.4");
        assert_eq!(port, 80);
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0x00]);
    }

    #[tokio::test]
    async fn socks5_v5_connect_domain() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let host = b"example.com";
        let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
        req.extend_from_slice(host);
        req.extend_from_slice(&443u16.to_be_bytes());
        client.write_all(&req).await.unwrap();
        let (host, port) = socks5_negotiate(&mut server).await.unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0x00]);
    }

    #[tokio::test]
    async fn socks5_v5_connect_ipv6() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut req = vec![0x05, 0x01, 0x00, 0x04];
        req.extend_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
        req.extend_from_slice(&22u16.to_be_bytes());
        client.write_all(&req).await.unwrap();
        let (host, port) = socks5_negotiate(&mut server).await.unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 22);
        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0x00]);
    }

    #[tokio::test]
    async fn socks5_rejects_bind_command() {
        let (mut client, mut server) = tokio::io::duplex(64);
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        client
            .write_all(&[0x05, 0x02, 0x00, 0x01, 1, 2, 3, 4, 0x00, 0x50])
            .await
            .unwrap();
        let err = socks5_negotiate(&mut server).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let mut method = [0u8; 2];
        client.read_exact(&mut method).await.unwrap();
        assert_eq!(method, [0x05, 0x00]);
        let mut rep = [0u8; 10];
        client.read_exact(&mut rep).await.unwrap();
        assert_eq!(rep[1], 0x07);
    }

    #[tokio::test]
    async fn bridge_propagates_data_and_eof_both_directions() {
        let (mut client_a, a) = tokio::io::duplex(64);
        let (b, mut server_b) = tokio::io::duplex(64);
        let bridged = tokio::spawn(async move { bridge(a, b).await });

        client_a.write_all(b"ping").await.unwrap();
        client_a.shutdown().await.unwrap();

        let mut got = Vec::new();
        server_b.read_to_end(&mut got).await.unwrap();
        assert_eq!(
            got, b"ping",
            "A→B data delivered and A-side EOF closed B read"
        );

        server_b.write_all(b"pong").await.unwrap();
        server_b.shutdown().await.unwrap();
        let mut back = Vec::new();
        client_a.read_to_end(&mut back).await.unwrap();
        assert_eq!(
            back, b"pong",
            "B→A reply delivered and B-side EOF closed A read"
        );

        bridged.await.unwrap().unwrap();
    }

    #[test]
    fn remote_forward_table_lookup() {
        let table = RemoteForwardTable::default();
        table.register("localhost", 9000, "127.0.0.1", 3000);
        assert_eq!(
            table.lookup("localhost", 9000),
            Some(("127.0.0.1".to_string(), 3000))
        );
        assert_eq!(
            table.lookup("127.0.0.1", 9000),
            Some(("127.0.0.1".to_string(), 3000))
        );
        assert_eq!(table.lookup("localhost", 9999), None);
        table.unregister("localhost", 9000);
        assert_eq!(table.lookup("localhost", 9000), None);
    }

    #[tokio::test]
    async fn registry_add_list_remove_teardown_bookkeeping() {
        let reg = SshForwardRegistry::default();
        let make = |id: u64, port: u16| {
            let task = tokio::spawn(async { std::future::pending::<()>().await });
            ForwardEntry {
                id,
                kind: SshForwardKind::Local,
                bind_host: "127.0.0.1".into(),
                bind_port: port,
                target_host: "h".into(),
                target_port: 80,
                description: None,
                status: ForwardStatus::Listening,
                cancel: ForwardCancel::Task(task),
                auto_local: false,
            }
        };
        {
            let mut owners = reg.owners.lock().unwrap();
            let entries = owners.entry(ForwardOwner::Pane(7)).or_default();
            entries.push(make(0, 8000));
            entries.push(make(1, 8001));
        }
        let list = reg.list(7);
        assert_eq!(list.iter().map(|m| m.id).collect::<Vec<_>>(), vec![0, 1]);
        assert!(reg.list(99).is_empty(), "other panes see nothing");

        let remaining = reg.remove(7, 0).await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, 1);

        reg.teardown_pane(7).await;
        assert!(reg.list(7).is_empty());
    }

    #[tokio::test]
    async fn remove_frees_listening_socket_synchronously() {
        async fn spawn_listener_entry(id: u64, guard: &Arc<()>) -> ForwardEntry {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let guard = guard.clone();
            let handle = tokio::spawn(async move {
                let _guard = guard;
                loop {
                    if listener.accept().await.is_err() {
                        break;
                    }
                }
            });
            ForwardEntry {
                id,
                kind: SshForwardKind::Local,
                bind_host: "127.0.0.1".into(),
                bind_port: port,
                target_host: "h".into(),
                target_port: 80,
                description: None,
                status: ForwardStatus::Listening,
                cancel: ForwardCancel::Task(handle),
                auto_local: false,
            }
        }

        let reg = SshForwardRegistry::default();

        let guard = Arc::new(());
        let entry = spawn_listener_entry(0, &guard).await;
        reg.owners
            .lock()
            .unwrap()
            .entry(ForwardOwner::Pane(1))
            .or_default()
            .push(entry);
        assert_eq!(
            Arc::strong_count(&guard),
            2,
            "task holds the socket while live"
        );
        reg.remove(1, 0).await;
        assert_eq!(
            Arc::strong_count(&guard),
            1,
            "remove() must drop the accept task (and its TcpListener) synchronously"
        );

        let guard2 = Arc::new(());
        let entry2 = spawn_listener_entry(1, &guard2).await;
        reg.owners
            .lock()
            .unwrap()
            .entry(ForwardOwner::Pane(2))
            .or_default()
            .push(entry2);
        reg.teardown_pane(2).await;
        assert_eq!(
            Arc::strong_count(&guard2),
            1,
            "teardown_pane() must drop every accept task synchronously"
        );
    }

    async fn push_listener(reg: &SshForwardRegistry, owner: ForwardOwner, id: u64) -> Arc<()> {
        let guard = Arc::new(());
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let held = guard.clone();
        let handle = tokio::spawn(async move {
            let _held = held;
            while listener.accept().await.is_ok() {}
        });
        let entry = ForwardEntry {
            id,
            kind: SshForwardKind::Local,
            bind_host: "127.0.0.1".into(),
            bind_port: port,
            target_host: "127.0.0.1".into(),
            target_port: 3000,
            description: None,
            status: ForwardStatus::Listening,
            cancel: ForwardCancel::Task(handle),
            auto_local: true,
        };
        reg.owners
            .lock()
            .unwrap()
            .entry(owner)
            .or_default()
            .push(entry);
        guard
    }

    #[tokio::test]
    async fn ssh_pane_forwards_die_with_the_pane() {
        let reg = SshForwardRegistry::default();
        let guard = push_listener(&reg, ForwardOwner::Pane(7), 0).await;
        assert_eq!(reg.list(7).len(), 1, "the pane owns its forward");

        reg.teardown_pane(7).await;

        assert!(reg.list(7).is_empty(), "the pane's forwards are gone");
        assert_eq!(
            Arc::strong_count(&guard),
            1,
            "and its listening socket was actually released"
        );
    }

    #[tokio::test]
    async fn remote_workspace_forwards_survive_their_panes() {
        let reg = SshForwardRegistry::default();
        let ws = WorkspaceId::new();
        let pane_guard = push_listener(&reg, ForwardOwner::Pane(7), 0).await;
        let ws_guard = push_listener(&reg, ForwardOwner::Workspace(ws), 1).await;

        reg.teardown_pane(7).await;

        assert!(reg.list(7).is_empty(), "the SSH pane's forward went away");
        assert_eq!(Arc::strong_count(&pane_guard), 1, "…and freed its socket");
        assert_eq!(
            reg.list_workspace(ws, 7).len(),
            1,
            "the workspace's forward is still there — the browser tab still works"
        );
        assert_eq!(
            Arc::strong_count(&ws_guard),
            2,
            "…and its listener is still bound"
        );

        reg.teardown_workspace(ws).await;
        assert!(reg.list_workspace(ws, 7).is_empty());
        assert_eq!(Arc::strong_count(&ws_guard), 1);
    }

    #[tokio::test]
    async fn workspaces_on_one_host_do_not_share_forwards() {
        let reg = SshForwardRegistry::default();
        let (a, b) = (WorkspaceId::new(), WorkspaceId::new());
        push_listener(&reg, ForwardOwner::Workspace(a), 0).await;
        push_listener(&reg, ForwardOwner::Workspace(b), 1).await;

        assert!(
            reg.find_auto_local(&ForwardOwner::Workspace(a), "127.0.0.1", 3000)
                .is_some()
        );
        assert!(
            reg.find_auto_local(&ForwardOwner::Pane(0), "127.0.0.1", 3000)
                .is_none(),
            "a pane never inherits a workspace's auto forward"
        );

        reg.teardown_workspace(a).await;
        assert!(reg.list_workspace(a, 0).is_empty());
        assert_eq!(
            reg.list_workspace(b, 0).len(),
            1,
            "the other window on the same host is untouched"
        );
    }

    #[test]
    fn remote_forward_table_rekey() {
        let table = RemoteForwardTable::default();
        table.register("", 0, "127.0.0.1", 3000);
        table.rekey("", 0, 40000);
        assert_eq!(
            table.lookup("", 40000),
            Some(("127.0.0.1".to_string(), 3000))
        );
        assert_eq!(table.lookup("", 0), None);
    }
}

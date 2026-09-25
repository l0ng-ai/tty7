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

/// A forward's status, shared with the task that serves it.
///
/// The status used to be a plain field written once when the forward was set
/// up and never again, so a forward whose accept loop had already exited went
/// on reporting `Listening` for as long as the pane stayed open — which is
/// forever, because a pane is deliberately not closed when its SSH connection
/// dies. The task that discovers the truth is the one that has to be able to
/// record it.
type SharedStatus = Arc<Mutex<ForwardStatus>>;

/// Why a forward's accept loop stopped.
enum LoopExit {
    /// `accept()` failed over and over: the listening socket is no longer
    /// usable, so nothing can even reach the forward any more.
    ListenerLost,
    /// The SSH transport went away under it. The port may still be bound —
    /// a connection to it is accepted and then dropped — but there is nothing
    /// on the far side of it.
    ConnectionLost,
}

/// The status a forward is left in once its loop has exited.
///
/// `ForwardStatus::Error` rather than a `Stopped` variant of its own, on
/// purpose: `ForwardStatus` is serialised across the protocol to remote
/// `tty7-server` builds, and a variant an older remote has never heard of is a
/// deserialisation failure rather than an unknown status. `Error` already
/// draws as a danger badge with its message beside it.
fn loop_exit_status(exit: LoopExit) -> ForwardStatus {
    ForwardStatus::Error(
        match exit {
            LoopExit::ListenerLost => "stopped: the listening socket closed",
            LoopExit::ConnectionLost => "stopped: the SSH connection went away",
        }
        .to_string(),
    )
}

/// The reason a just-started forward is not listening, if it is not.
///
/// Only ever a bind failure at this point: everything else that can stop a
/// forward happens later, in the accept loop.
fn bind_error(status: &SharedStatus) -> Option<String> {
    match &*status.lock().unwrap() {
        ForwardStatus::Error(e) => Some(e.clone()),
        ForwardStatus::Listening => None,
    }
}

/// A forward that never got as far as a listening socket. It has no task, so
/// nothing will ever move it off this status.
fn bind_failed(rule: &SshForwardRule, e: io::Error) -> SharedStatus {
    Arc::new(Mutex::new(ForwardStatus::Error(format!(
        "bind {}:{} failed: {e}",
        rule.bind_host, rule.bind_port
    ))))
}

/// What a switched-off forward reports. An `Error` for the same reason as
/// `loop_exit_status`, so an older peer reading it draws a stopped forward
/// with a word of explanation; a current one reads `enabled` instead.
fn disabled_status() -> SharedStatus {
    Arc::new(Mutex::new(ForwardStatus::Error("disabled".to_string())))
}

/// Whether two forwards would want the same listening socket: the same port,
/// on the same side of the connection. Local and dynamic forwards both listen
/// here; a remote forward listens on the far host.
fn same_socket(a: SshForwardKind, a_port: u16, b: SshForwardKind, b_port: u16) -> bool {
    let far = |k| k == SshForwardKind::Remote;
    a_port != 0 && a_port == b_port && far(a) == far(b)
}

struct ForwardEntry {
    id: u64,
    kind: SshForwardKind,
    bind_host: String,
    bind_port: u16,
    /// The port the rule asked for, which `bind_port` is not when that was 0
    /// and the OS picked one. Switching the forward back on asks again for
    /// this, not for whatever it happened to get last time.
    requested_port: u16,
    target_host: String,
    target_port: u16,
    description: Option<String>,
    status: SharedStatus,
    cancel: ForwardCancel,
    auto_local: bool,
    /// Switched off: the entry, and the rule in it, stay listed, but nothing
    /// is bound and `cancel` is `None`.
    enabled: bool,
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
            status: self.status.lock().unwrap().clone(),
            enabled: self.enabled,
        }
    }

    fn rule(&self) -> SshForwardRule {
        SshForwardRule {
            kind: self.kind,
            bind_host: self.bind_host.clone(),
            bind_port: self.requested_port,
            target_host: self.target_host.clone(),
            target_port: self.target_port,
            description: self.description.clone(),
            enabled: self.enabled,
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

    pub async fn set_enabled(
        &self,
        pane_id: u64,
        conn: Option<Arc<SshConnection>>,
        forward_id: u64,
        enabled: bool,
    ) -> Result<Vec<ManagedForward>, String> {
        self.set_enabled_owned(
            &ForwardOwner::Pane(pane_id),
            pane_id,
            conn,
            forward_id,
            enabled,
        )
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

    pub async fn set_enabled_workspace(
        &self,
        workspace: WorkspaceId,
        view_pane: u64,
        conn: Arc<SshConnection>,
        forward_id: u64,
        enabled: bool,
    ) -> Result<Vec<ManagedForward>, String> {
        self.set_enabled_owned(
            &ForwardOwner::Workspace(workspace),
            view_pane,
            Some(conn),
            forward_id,
            enabled,
        )
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
        let (bind_port, status, cancel) = if rule.enabled {
            self.start(&conn, rule).await
        } else {
            (rule.bind_port, disabled_status(), ForwardCancel::None)
        };
        let entry = ForwardEntry {
            id,
            kind: rule.kind,
            bind_host: rule.bind_host.clone(),
            bind_port,
            requested_port: rule.bind_port,
            target_host: rule.target_host.clone(),
            target_port: rule.target_port,
            description: rule.description.clone(),
            status,
            cancel,
            auto_local: false,
            enabled: rule.enabled,
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
            Self::cancel(entry.cancel).await;
        }
        self.list_owned(owner, view_pane)
    }

    /// Switch one forward off or back on, keeping its entry — and so its
    /// place in the list and the rule it was made from — either way.
    ///
    /// Switching on refuses, rather than fails at bind time with whatever the
    /// OS says, when another switched-on forward of the same owner already
    /// holds the port: the answer then names that forward, which is the one
    /// the user has to switch off first. A forward that cannot be started for
    /// any other reason stays off, with the reason as the error.
    async fn set_enabled_owned(
        &self,
        owner: &ForwardOwner,
        view_pane: u64,
        conn: Option<Arc<SshConnection>>,
        forward_id: u64,
        enabled: bool,
    ) -> Result<Vec<ManagedForward>, String> {
        if !enabled {
            let cancel = {
                let mut owners = self.owners.lock().unwrap();
                let entry = Self::entry_mut(&mut owners, owner, forward_id)?;
                entry.enabled = false;
                entry.status = disabled_status();
                std::mem::replace(&mut entry.cancel, ForwardCancel::None)
            };
            Self::cancel(cancel).await;
            return Ok(self.list_owned(owner, view_pane));
        }

        let Some(rule) = self.enable_plan(owner, forward_id)? else {
            return Ok(self.list_owned(owner, view_pane));
        };
        let Some(conn) = conn else {
            return Err("the SSH connection is gone — reconnect and try again".to_string());
        };
        let (bound, status, cancel) = self.start(&conn, &rule).await;
        if let Some(e) = bind_error(&status) {
            Self::cancel(cancel).await;
            return Err(e);
        }
        if let Some(cancel) = self.finish_enable(owner, forward_id, bound, status, cancel) {
            // Removed while it was being started: nobody wants what was bound.
            Self::cancel(cancel).await;
        }
        Ok(self.list_owned(owner, view_pane))
    }

    fn entry_mut<'a>(
        owners: &'a mut HashMap<ForwardOwner, Vec<ForwardEntry>>,
        owner: &ForwardOwner,
        forward_id: u64,
    ) -> Result<&'a mut ForwardEntry, String> {
        owners
            .get_mut(owner)
            .and_then(|entries| entries.iter_mut().find(|e| e.id == forward_id))
            .ok_or_else(|| format!("no forward {forward_id}"))
    }

    /// The rule to start for switching `forward_id` on, `None` when it is on
    /// already, or why it may not be switched on.
    fn enable_plan(
        &self,
        owner: &ForwardOwner,
        forward_id: u64,
    ) -> Result<Option<SshForwardRule>, String> {
        let mut owners = self.owners.lock().unwrap();
        let entry = Self::entry_mut(&mut owners, owner, forward_id)?;
        if entry.enabled {
            return Ok(None);
        }
        let rule = SshForwardRule {
            enabled: true,
            ..entry.rule()
        };
        let holder = owners.get(owner).into_iter().flatten().find(|e| {
            e.id != forward_id
                && e.enabled
                && same_socket(e.kind, e.bind_port, rule.kind, rule.bind_port)
        });
        if let Some(h) = holder {
            let what = match h.kind {
                SshForwardKind::Dynamic => "a SOCKS forward".to_string(),
                _ => format!("the forward to {}:{}", h.target_host, h.target_port),
            };
            return Err(format!(
                "port {} is already used by {what}; switch that one off first",
                rule.bind_port
            ));
        }
        Ok(Some(rule))
    }

    /// Record a started forward as switched on. Hands `cancel` back when the
    /// entry is no longer there to take it.
    fn finish_enable(
        &self,
        owner: &ForwardOwner,
        forward_id: u64,
        bound: u16,
        status: SharedStatus,
        cancel: ForwardCancel,
    ) -> Option<ForwardCancel> {
        let mut owners = self.owners.lock().unwrap();
        let Ok(entry) = Self::entry_mut(&mut owners, owner, forward_id) else {
            return Some(cancel);
        };
        entry.enabled = true;
        entry.bind_port = bound;
        entry.status = status;
        // Anything already in the slot is a second switch-on that raced this
        // one; it is replaced, and has to be stopped rather than leaked.
        Some(std::mem::replace(&mut entry.cancel, cancel))
    }

    async fn start(
        &self,
        conn: &Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> (u16, SharedStatus, ForwardCancel) {
        match rule.kind {
            SshForwardKind::Local => self.start_local(conn, rule).await,
            SshForwardKind::Dynamic => self.start_dynamic(conn, rule).await,
            SshForwardKind::Remote => self.start_remote(conn, rule).await,
        }
    }

    async fn teardown_owned(&self, owner: &ForwardOwner) {
        let entries = self.owners.lock().unwrap().remove(owner);
        for entry in entries.into_iter().flatten() {
            Self::cancel(entry.cancel).await;
        }
    }

    async fn cancel(cancel: ForwardCancel) {
        match cancel {
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
    ) -> (u16, SharedStatus, ForwardCancel) {
        let listener = match TcpListener::bind((rule.bind_host.as_str(), rule.bind_port)).await {
            Ok(l) => l,
            Err(e) => {
                return (rule.bind_port, bind_failed(rule, e), ForwardCancel::None);
            }
        };
        let bound = listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(rule.bind_port);
        let conn = conn.clone();
        let target_host = rule.target_host.clone();
        let target_port = rule.target_port;
        let status: SharedStatus = Arc::new(Mutex::new(ForwardStatus::Listening));
        let task_status = status.clone();
        let handle = tokio::spawn(async move {
            let exit = loop {
                let sock = match accept_retrying(&listener).await {
                    Some((sock, _peer)) => sock,
                    None => break LoopExit::ListenerLost,
                };
                if !conn.is_alive() {
                    break LoopExit::ConnectionLost;
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
            };
            *task_status.lock().unwrap() = loop_exit_status(exit);
        });
        (bound, status, ForwardCancel::Task(handle))
    }

    async fn start_dynamic(
        &self,
        conn: &Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> (u16, SharedStatus, ForwardCancel) {
        let listener = match TcpListener::bind((rule.bind_host.as_str(), rule.bind_port)).await {
            Ok(l) => l,
            Err(e) => {
                return (rule.bind_port, bind_failed(rule, e), ForwardCancel::None);
            }
        };
        let bound = listener
            .local_addr()
            .map(|a| a.port())
            .unwrap_or(rule.bind_port);
        let conn = conn.clone();
        let status: SharedStatus = Arc::new(Mutex::new(ForwardStatus::Listening));
        let task_status = status.clone();
        let handle = tokio::spawn(async move {
            let exit = loop {
                let sock = match accept_retrying(&listener).await {
                    Some((sock, _peer)) => sock,
                    None => break LoopExit::ListenerLost,
                };
                if !conn.is_alive() {
                    break LoopExit::ConnectionLost;
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
            };
            *task_status.lock().unwrap() = loop_exit_status(exit);
        });
        (bound, status, ForwardCancel::Task(handle))
    }

    /// Unlike the two above, a remote forward has no accept loop of its own to
    /// notice a dead transport: the far end opens the channels and russh hands
    /// them to the connection's handler. Its status stays whatever the
    /// `tcpip-forward` request answered.
    async fn start_remote(
        &self,
        conn: &Arc<SshConnection>,
        rule: &SshForwardRule,
    ) -> (u16, SharedStatus, ForwardCancel) {
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
                Arc::new(Mutex::new(ForwardStatus::Listening)),
                ForwardCancel::Remote {
                    conn: Arc::downgrade(conn),
                    bind_host: rule.bind_host.clone(),
                    bind_port: bound,
                },
            ),
            Err(e) => (
                rule.bind_port,
                Arc::new(Mutex::new(ForwardStatus::Error(format!(
                    "remote forward request denied: {e}"
                )))),
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
        let mut rule = SshForwardRule {
            kind: SshForwardKind::Local,
            bind_host: "127.0.0.1".to_string(),
            // The far side's own number first. An automatic forward used to
            // always bind 0, so the remote's :3000 came out on a different
            // five-digit port every session — an address nobody could predict,
            // guess or bookmark. When the number is free here, keeping it makes
            // localhost:3000 mean what it says.
            bind_port: remote_port,
            target_host: remote_host.to_string(),
            target_port: remote_port,
            description: Some(format!("localhost link → :{remote_port}")),
            enabled: true,
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (mut bind_port, mut status, mut cancel) = self.start_local(&conn, &rule).await;
        // Taken here, or a privileged port this process may not bind. Neither
        // is a reason to fail: 0 asks the OS for one that works, which is what
        // this did before it tried for the matching number.
        if let Some(e) = bind_error(&status) {
            if rule.bind_port == 0 {
                return Err(io::Error::other(e));
            }
            log::debug!(
                "loopback forward could not keep :{remote_port} locally ({e}); asking the OS for a port"
            );
            rule.bind_port = 0;
            (bind_port, status, cancel) = self.start_local(&conn, &rule).await;
            if let Some(e) = bind_error(&status) {
                return Err(io::Error::other(e));
            }
        }
        let entry = ForwardEntry {
            id,
            kind: SshForwardKind::Local,
            bind_host: rule.bind_host.clone(),
            bind_port,
            requested_port: rule.bind_port,
            target_host: rule.target_host.clone(),
            target_port: rule.target_port,
            description: rule.description.clone(),
            status,
            cancel,
            auto_local: true,
            enabled: true,
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
                    // Now that a dead loop says so, this stops handing out the
                    // port of a forward that no longer serves anything.
                    && matches!(*e.status.lock().unwrap(), ForwardStatus::Listening)
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

    pub fn set_workspace_forward_enabled(
        &self,
        workspace: WorkspaceId,
        view_pane: u64,
        conn: Arc<SshConnection>,
        forward_id: u64,
        enabled: bool,
    ) -> Result<Vec<ManagedForward>, String> {
        self.runtime.block_on(
            self.forwards
                .set_enabled_workspace(workspace, view_pane, conn, forward_id, enabled),
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

    #[test]
    fn a_stopped_forward_says_why_it_stopped() {
        let listener = loop_exit_status(LoopExit::ListenerLost);
        let connection = loop_exit_status(LoopExit::ConnectionLost);
        assert_ne!(
            listener, connection,
            "a socket that closed and a transport that died are different problems"
        );
        for status in [&listener, &connection] {
            assert!(
                matches!(status, ForwardStatus::Error(_)),
                "an older remote has to be able to deserialise this, so no new variant"
            );
        }
    }

    /// A forward's status used to be copied into `ManagedForward` from a field
    /// written once when the forward was set up, so a forward whose loop had
    /// long since exited answered `Listening` to every poll of the panel.
    #[tokio::test]
    async fn a_forward_whose_loop_exited_stops_reporting_listening() {
        let reg = SshForwardRegistry::default();
        push_listener(&reg, ForwardOwner::Pane(7), 0).await;
        assert!(matches!(reg.list(7)[0].status, ForwardStatus::Listening));

        // What the accept loop does on its way out. The entry is already in the
        // registry by then, which is the whole reason the status is shared.
        let status = reg.owners.lock().unwrap()[&ForwardOwner::Pane(7)][0]
            .status
            .clone();
        *status.lock().unwrap() = loop_exit_status(LoopExit::ConnectionLost);

        match &reg.list(7)[0].status {
            ForwardStatus::Error(msg) => assert!(
                msg.contains("SSH connection"),
                "the panel needs a reason, not just a red badge: {msg}"
            ),
            other => panic!("a dead forward still reports {other:?}"),
        }
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
                requested_port: port,
                target_host: "h".into(),
                target_port: 80,
                description: None,
                status: Arc::new(Mutex::new(ForwardStatus::Listening)),
                cancel: ForwardCancel::Task(task),
                auto_local: false,
                enabled: true,
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
                requested_port: port,
                target_host: "h".into(),
                target_port: 80,
                description: None,
                status: Arc::new(Mutex::new(ForwardStatus::Listening)),
                cancel: ForwardCancel::Task(handle),
                auto_local: false,
                enabled: true,
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
            requested_port: port,
            target_host: "127.0.0.1".into(),
            target_port: 3000,
            description: None,
            status: Arc::new(Mutex::new(ForwardStatus::Listening)),
            cancel: ForwardCancel::Task(handle),
            auto_local: true,
            enabled: true,
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

    fn push_disabled(
        reg: &SshForwardRegistry,
        owner: ForwardOwner,
        id: u64,
        kind: SshForwardKind,
        port: u16,
    ) {
        reg.owners
            .lock()
            .unwrap()
            .entry(owner)
            .or_default()
            .push(ForwardEntry {
                id,
                kind,
                bind_host: "127.0.0.1".into(),
                bind_port: port,
                requested_port: port,
                target_host: "srv-b".into(),
                target_port: 80,
                description: None,
                status: disabled_status(),
                cancel: ForwardCancel::None,
                auto_local: false,
                enabled: false,
            });
    }

    /// Switching a forward off has to release what it bound — that is the
    /// point, so the port can be taken by another rule — while leaving the
    /// entry, and the rule in it, exactly where it was.
    #[tokio::test]
    async fn switching_a_forward_off_frees_its_socket_and_keeps_its_rule() {
        let reg = SshForwardRegistry::default();
        let guard = push_listener(&reg, ForwardOwner::Pane(7), 3).await;
        let port = reg.list(7)[0].bind_port;

        let list = reg.set_enabled(7, None, 3, false).await.unwrap();
        assert_eq!(list.len(), 1, "the forward is still listed");
        assert_eq!(list[0].id, 3);
        assert!(!list[0].enabled);
        assert_eq!(list[0].bind_port, port);
        assert_eq!(
            (list[0].target_host.as_str(), list[0].target_port),
            ("127.0.0.1", 3000)
        );
        assert!(matches!(list[0].status, ForwardStatus::Error(_)));
        assert_eq!(Arc::strong_count(&guard), 1, "…and its listener is gone");
        TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("the port is free for someone else");

        // Twice is the same as once, and a forward nobody has is an error.
        assert_eq!(reg.set_enabled(7, None, 3, false).await.unwrap(), list);
        assert!(reg.set_enabled(7, None, 99, false).await.is_err());

        reg.teardown_pane(7).await;
        assert!(reg.list(7).is_empty());
    }

    /// Two rules on one local port, pointed at different targets, is the
    /// whole use of the switch. Turning the second on while the first still
    /// holds the port is refused by name, not left to fail at bind time.
    #[tokio::test]
    async fn switching_on_a_forward_whose_port_is_held_is_refused_by_name() {
        let reg = SshForwardRegistry::default();
        push_listener(&reg, ForwardOwner::Pane(7), 0).await;
        let port = reg.list(7)[0].bind_port;
        push_disabled(&reg, ForwardOwner::Pane(7), 1, SshForwardKind::Local, port);

        let err = reg.set_enabled(7, None, 1, true).await.unwrap_err();
        assert!(err.contains(&port.to_string()), "{err}");
        assert!(err.contains("127.0.0.1:3000"), "names the holder: {err}");
        assert!(!reg.list(7)[1].enabled, "the refused forward stays off");

        // A SOCKS forward listens here too, so it collides the same way.
        push_disabled(
            &reg,
            ForwardOwner::Pane(7),
            2,
            SshForwardKind::Dynamic,
            port,
        );
        assert!(reg.set_enabled(7, None, 2, true).await.is_err());

        // Once the holder is off, the only thing left in the way is the
        // missing connection — which is said, and changes nothing.
        reg.set_enabled(7, None, 0, false).await.unwrap();
        let err = reg.set_enabled(7, None, 1, true).await.unwrap_err();
        assert!(err.contains("connection"), "{err}");
        assert!(reg.list(7).iter().all(|m| !m.enabled));

        // Another owner's forwards are not this one's business.
        push_disabled(&reg, ForwardOwner::Pane(8), 0, SshForwardKind::Local, port);
        assert!(
            reg.enable_plan(&ForwardOwner::Pane(8), 0)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn only_the_same_port_on_the_same_side_collides() {
        use SshForwardKind::{Dynamic, Local, Remote};
        assert!(same_socket(Local, 8080, Local, 8080));
        assert!(same_socket(Local, 8080, Dynamic, 8080));
        assert!(same_socket(Remote, 8080, Remote, 8080));
        assert!(!same_socket(Local, 8080, Remote, 8080), "different hosts");
        assert!(!same_socket(Local, 8080, Local, 8081));
        assert!(
            !same_socket(Local, 0, Local, 0),
            "0 asks the OS for a fresh one"
        );
    }

    /// Switching back on asks for the port the rule asked for, not the one
    /// the OS last picked for it, and records what it got this time.
    #[tokio::test]
    async fn switching_on_starts_from_the_rule_and_records_the_new_listener() {
        let reg = SshForwardRegistry::default();
        push_disabled(&reg, ForwardOwner::Pane(7), 0, SshForwardKind::Local, 0);
        reg.owners
            .lock()
            .unwrap()
            .get_mut(&ForwardOwner::Pane(7))
            .unwrap()[0]
            .bind_port = 41000;

        let rule = reg
            .enable_plan(&ForwardOwner::Pane(7), 0)
            .unwrap()
            .expect("an off forward has something to start");
        assert_eq!(rule.bind_port, 0);
        assert!(rule.enabled);
        assert_eq!((rule.target_host.as_str(), rule.target_port), ("srv-b", 80));

        let guard = Arc::new(());
        let held = guard.clone();
        let task = tokio::spawn(async move {
            let _held = held;
            std::future::pending::<()>().await
        });
        let status: SharedStatus = Arc::new(Mutex::new(ForwardStatus::Listening));
        let leftover = reg.finish_enable(
            &ForwardOwner::Pane(7),
            0,
            42000,
            status,
            ForwardCancel::Task(task),
        );
        assert!(matches!(leftover, Some(ForwardCancel::None)));
        let now = &reg.list(7)[0];
        assert!(now.enabled);
        assert_eq!(now.bind_port, 42000);
        assert_eq!(now.status, ForwardStatus::Listening);
        assert!(
            reg.enable_plan(&ForwardOwner::Pane(7), 0)
                .unwrap()
                .is_none(),
            "an on forward has nothing to start"
        );

        reg.teardown_pane(7).await;
        assert_eq!(Arc::strong_count(&guard), 1, "the new listener is owned");
    }
}

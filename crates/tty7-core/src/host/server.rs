//! The control dialect's **server** half: [`ControlRequest`] in, [`Host`] out.
//!
//! This is the other end of [`RemoteHost`](crate::host::remote::RemoteHost).
//! One connection arrives, says hello, and then issues filesystem, git and watch
//! requests that this module runs against a `Host` — in practice the
//! [`LocalHost`](crate::host::local::LocalHost) of the machine the server is on.
//! The client sees a `Host`; the server *uses* a `Host`; the wire in between
//! carries nothing else.
//!
//! That symmetry is not an accident, it is the reason the trait is blocking
//!. A server handler runs on a thread pool, where blocking is what
//! you want, so the identical `LocalHost` that answers a local file tree answers
//! a remote one — no async mirror of every method, and no second implementation
//! to drift.
//!
//! # Out-of-order is the point
//!
//! Requests are dispatched onto a pool and replied to as they finish, in
//! whatever order that turns out to be. A serial loop would be far simpler and
//! completely useless: a `git status` on a cold monorepo takes tens of seconds,
//! and behind a serial server every disclosure triangle in the file tree would
//! wait for it. `req_id` exists precisely so a reply can arrive out of turn, and
//! this side has to actually produce that — the client's multiplexer is only
//! half of the guarantee.
//!
//! The pool is elastic rather than fixed for the same reason. A fixed pool of
//! `N` serializes at `N` concurrent slow requests, which just moves the stall
//! rather than removing it; workers here are spawned on demand up to
//! [`MAX_WORKERS`] and retire after [`WORKER_LINGER`] of idleness, so the common
//! case (a burst of `stat`s) costs one or two threads and the pathological case
//! (sixty simultaneous `git`s) still answers the sixty-first `stat` immediately.
//!
//! # What a connection owns
//!
//! | Thing | Lifetime |
//! |---|---|
//! | The reply sink | The connection. One mutex, one whole frame per lock, so replies never interleave |
//! | In-flight ids | Per request. Records cancellation and is erased when the reply goes out |
//! | Watch subscriptions | Until `WatchClose`, or until the connection ends — whichever comes first, so a dropped link cannot leak a server-side watcher |
//! | Pool workers | Shared per connection; retired on idle, cleared on teardown |

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::core::workspace_store::{Attachment, SubscriberId, Subscription, WorkspaceStore};
use crate::daemon::control::{
    CONTROL_VERSION, ControlClientMsg, ControlEvent, ControlHello, ControlHelloOk, ControlReply,
    ControlRequest, ControlServerMsg, LinkShutdown, ReplyOk, WATCH_BURST_CAP, WireError,
    WireErrorKind, feature,
};
use crate::daemon::duplex::{Duplex, Halves};
use crate::host::{Host, SearchHit, SharedHost, WatchSub};

/// Ceiling on threads one connection's pool will grow to.
///
/// High enough that no realistic client can serialize behind it — the file tree
/// issues a few dozen listings per frame at worst — and low enough that a
/// runaway or hostile peer cannot turn request volume into thread count.
pub const MAX_WORKERS: usize = 64;

/// How long an idle worker waits for more work before retiring. A burst of
/// listings should not leave sixty threads parked for the rest of the session,
/// and a steady trickle should not pay a spawn per request.
pub const WORKER_LINGER: Duration = Duration::from_secs(10);

/// Requests allowed to queue once every worker is busy. Past this the server
/// answers immediately rather than growing an unbounded backlog whose entries
/// would each be answered long after the client's own deadline gave up on them.
pub const MAX_QUEUED: usize = 1024;

/// `WorkspaceChanged` pushes one connection will let pile up before it starts
/// dropping them.
///
/// Dropping is safe here in a way it is not for a watch batch: the event says
/// only "workspace `id` changed, refetch", so a client that has one queued
/// already learns everything a second one would tell it. The cap exists so a
/// peer that has stopped reading its socket cannot turn another client's
/// `WorkspacePut` into unbounded memory.
pub const WORKSPACE_EVENT_QUEUE: usize = 64;

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// What a control server offers **beyond** the host itself.
///
/// Separate from the `SharedHost` argument because the two are genuinely
/// independent roles, and the handshake says so: a box can back a remote
/// workspace's file tree without owning any workspace records (that is every
/// server today, and it is what [`Services::default`] produces), and the
/// `workspace-store` capability bit is advertised only when this actually
/// carries a store. A client therefore learns from the handshake whether asking
/// is worth a round trip.
#[derive(Clone, Default)]
pub struct Services {
    /// The machine's workspace records. `None` answers every `Workspace*`
    /// request with "this server does not serve the workspace store" — the same
    /// answer a build from before M5 gives.
    pub workspaces: Option<Arc<WorkspaceStore>>,
    /// Who currently holds each workspace, and how to reach them. Shared across
    /// every connection this server accepts — that sharing *is* the takeover:
    /// two connections can only displace each other if they are looking at one
    /// table.
    pub attachments: Arc<AttachRegistry>,
}

impl Services {
    /// Host RPC only, no workspace store.
    pub fn none() -> Services {
        Services::default()
    }

    /// Host RPC plus the workspace store.
    pub fn with_workspaces(store: Arc<WorkspaceStore>) -> Services {
        Services {
            workspaces: Some(store),
            attachments: Arc::new(AttachRegistry::default()),
        }
    }
}

// ---------------------------------------------------------------------------
// Attachment / takeover (D8)
// ---------------------------------------------------------------------------

/// The live half of the attachment record.
///
/// [`Attachment`](crate::core::workspace_store::Attachment) in the store is the
/// *data* — token, hostname, since — and answers "who holds this workspace".
/// This is the *handles*: the sink a `Preempted` push goes out on and the
/// shutdown that closes the displaced session's link. They are separate because
/// the store lives in `core` and knows nothing about sockets, and because an
/// attachment must never be written to the file (a stale one on disk would have
/// the server report a takeover against a client that no longer exists).
///
/// **D8's rule, concretely**: the newcomer always wins. The previous holder is
/// told and let go, never refused — "the old machine forgot to close the
/// window" is the common case, and rejecting the new client locks the user out
/// of their own box.
#[derive(Default)]
pub struct AttachRegistry {
    live: Mutex<Vec<Live>>,
    /// Held across *both* tables for the length of one handover.
    ///
    /// A takeover moves two things that live in different places: this
    /// registry's handles, and the `WorkspaceStore`'s record. Each is
    /// internally locked, and that is not enough — two clients attaching to one
    /// workspace at the same moment can each win a different table, after which
    /// the store names a session the registry has already evicted and no
    /// `detach` can ever clear it, because the token no longer matches. From
    /// then on the workspace reports a takeover against a client that
    /// disconnected hours ago.
    ///
    /// Coarse on purpose: attach and detach happen once per workspace opened or
    /// closed, so serializing them costs nothing worth measuring. Always the
    /// outermost lock of the two, and never held while writing to a peer.
    handover: Mutex<()>,
}

struct Live {
    workspace: String,
    /// The connection holding it. Compared before evicting, so re-attaching from
    /// the same connection is a no-op rather than a self-takeover.
    conn: u64,
    token: String,
    hostname: String,
    sink: Arc<Sink>,
    shutdown: Arc<dyn LinkShutdown>,
    /// The link was opened *for* this workspace — its
    /// [`ControlHello::workspace`] named it. See [`Evicted::dedicated`].
    dedicated: bool,
}

/// A session that has just been displaced, and how to tell it so.
struct Evicted {
    hostname: String,
    sink: Arc<Sink>,
    shutdown: Arc<dyn LinkShutdown>,
    /// Whether the link exists *for* this workspace, and so should be closed
    /// with it.
    ///
    /// The displaced session's streams are closed. When the
    /// connection was opened for one workspace — its hello named it — that is
    /// exactly right, and it is the strong form of the guarantee: the old client
    /// cannot write again even if it is wedged or hostile.
    ///
    /// A connection that attached by *request* is multiplexing by construction:
    /// the client has told us this link carries more than one thing, and a
    /// client holds one connection per **machine**. Closing it would take
    /// windows nobody preempted down with the one that was, so that link keeps
    /// running and only loses the workspace. The push goes out either way.
    dedicated: bool,
}

impl AttachRegistry {
    /// Take the handover lock. See [`AttachRegistry::handover`].
    fn handover(&self) -> std::sync::MutexGuard<'_, ()> {
        self.handover.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Who holds `workspace` — `(token, hostname)`. Diagnostics and tests.
    pub fn holder(&self, workspace: &str) -> Option<(String, String)> {
        self.locked()
            .iter()
            .find(|l| l.workspace == workspace)
            .map(|l| (l.token.clone(), l.hostname.clone()))
    }

    /// How many workspaces are attached right now.
    pub fn len(&self) -> usize {
        self.locked().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take `workspace` for `conn`, answering whoever it was taken from.
    ///
    /// One lock covers the eviction and the insert: a third client arriving
    /// mid-takeover must end up displacing exactly one of the two, never both
    /// and never neither.
    fn claim(
        &self,
        workspace: &str,
        conn: u64,
        holder: &Holder,
        dedicated: bool,
    ) -> Option<Evicted> {
        let mut live = self.locked();
        let evicted = match live.iter().position(|l| l.workspace == workspace) {
            Some(i) if live[i].conn == conn => {
                // The same connection re-attaching. Refresh it and tell nobody:
                // a client must not be able to preempt itself.
                live[i].token = holder.token.clone();
                None
            }
            Some(i) => {
                let old = live.remove(i);
                Some(Evicted {
                    hostname: old.hostname,
                    sink: old.sink,
                    shutdown: old.shutdown,
                    dedicated: old.dedicated,
                })
            }
            None => None,
        };
        if !live.iter().any(|l| l.workspace == workspace) {
            live.push(Live {
                workspace: workspace.to_string(),
                conn,
                token: holder.token.clone(),
                hostname: holder.hostname.clone(),
                sink: Arc::clone(&holder.sink),
                shutdown: Arc::clone(&holder.shutdown),
                dedicated,
            });
        }
        evicted
    }

    /// Forget `workspace` whoever holds it — the workspace itself is gone.
    ///
    /// Unconditional, unlike [`AttachRegistry::release`]: a delete is not one
    /// session giving something up, it is the thing ceasing to exist.
    fn forget_workspace(&self, workspace: &str) {
        self.locked().retain(|l| l.workspace != workspace);
    }

    /// Release `workspace`, but only if `conn` still holds it. `false` means it
    /// had already been taken over, which is success as far as the caller is
    /// concerned — and the reason releasing is conditional at all.
    fn release(&self, workspace: &str, conn: u64) -> bool {
        let mut live = self.locked();
        let before = live.len();
        live.retain(|l| !(l.workspace == workspace && l.conn == conn));
        live.len() != before
    }

    /// Release everything `conn` holds, naming what was released so the store's
    /// records can be dropped too.
    fn release_conn(&self, conn: u64) -> Vec<String> {
        let mut live = self.locked();
        let mut released = Vec::new();
        live.retain(|l| {
            if l.conn == conn {
                released.push(l.workspace.clone());
                false
            } else {
                true
            }
        });
        released
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Vec<Live>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The identity one connection attaches under.
struct Holder {
    token: String,
    hostname: String,
    sink: Arc<Sink>,
    shutdown: Arc<dyn LinkShutdown>,
}

/// Serve one control connection to completion.
///
/// Returns when the peer hangs up, when a frame does not decode, or when the
/// link is shut down from elsewhere. Every resource the connection acquired —
/// watches, workers, the workspace subscription, the write half — is released
/// before this returns.
pub fn serve<D: Duplex>(link: D, host: SharedHost) -> io::Result<()> {
    serve_with(link, host, Services::none())
}

/// [`serve`], with the extra services this server offers.
pub fn serve_with<D: Duplex>(link: D, host: SharedHost, services: Services) -> io::Result<()> {
    let label = link.kind_label();
    let Halves {
        read,
        write,
        shutdown,
    } = link.split()?;
    serve_halves_with(read, write, shutdown, host, services, label)
}

/// [`serve`] over halves that have already been split.
///
/// Exists for callers holding two unrelated handles — the stdio server, and
/// tests driving a socket pair — which is the same reason
/// [`ControlClient::connect`](crate::daemon::control::ControlClient::connect)
/// takes its halves separately on the other side.
pub fn serve_halves<R, W>(
    r: R,
    w: W,
    shutdown: Arc<dyn LinkShutdown>,
    host: SharedHost,
    label: &'static str,
) -> io::Result<()>
where
    R: Read,
    W: Write + Send + 'static,
{
    serve_halves_with(r, w, shutdown, host, Services::none(), label)
}

/// [`serve_halves`], with the extra services this server offers.
pub fn serve_halves_with<R, W>(
    mut r: R,
    w: W,
    shutdown: Arc<dyn LinkShutdown>,
    host: SharedHost,
    services: Services,
    label: &'static str,
) -> io::Result<()>
where
    R: Read,
    W: Write + Send + 'static,
{
    let sink = Arc::new(Sink::new(w));

    // The handshake happens on this thread, before anything is spawned: a peer
    // that speaks a different dialect must cost exactly one frame each way.
    //
    // A peer that hangs up *during* the handshake is a disconnect like any
    // other, not a failure worth reporting — a port scanner, a health check, or
    // a client that changed its mind all land here, and none of them are this
    // server's problem.
    let hello = match handshake(&mut r, &sink, &*host, &services) {
        Ok(Some(hello)) => hello,
        Ok(None) => {
            let _ = shutdown.shutdown_link();
            return Ok(());
        }
        Err(e) => {
            let _ = shutdown.shutdown_link();
            return if is_disconnect(&e) {
                log::debug!("control peer ({label}) left during the handshake: {e}");
                Ok(())
            } else {
                Err(e)
            };
        }
    };

    // Subscribed before the first request is read, so a change another client
    // makes while this one is still listing cannot slip through the gap.
    let workspace_sub = subscribe_workspaces(&services, &sink);

    let conn = Arc::new(Conn {
        host,
        sink: Arc::clone(&sink),
        inflight: Mutex::new(HashMap::new()),
        watches: Mutex::new(HashMap::new()),
        deferred_watches: Mutex::new(HashMap::new()),
        next_watch: AtomicU64::new(1),
        pool: Pool::new(),
        workspaces: services.workspaces.clone(),
        workspace_origin: workspace_sub.as_ref().map(Subscription::id),
        attachments: Arc::clone(&services.attachments),
        id: NEXT_CONN.fetch_add(1, Ordering::Relaxed),
        holder: Holder {
            token: hello.client_token.clone(),
            hostname: hello.client_hostname.clone(),
            sink: Arc::clone(&sink),
            shutdown: Arc::clone(&shutdown),
        },
    });

    // A hello that names a workspace attaches to it straight away, before the
    // first request is read: the client that opened a connection *for* a
    // workspace has already taken it over, and a window between the handshake
    // and an explicit `WorkspaceAttach` is a window in which two clients both
    // believe they hold it.
    if let Some(workspace) = hello.workspace.as_deref()
        && let Err(e) = attach_workspace(&conn, workspace, true)
    {
        log::warn!("control peer ({label}) could not attach to workspace {workspace}: {e}");
    }

    let outcome = read_loop(&mut r, &conn);

    // Teardown, in the order that makes each step meaningful: stop accepting
    // work, drop the watches (which stops the pushes and releases the OS
    // watchers), release anything this session still holds, drop the workspace
    // subscription (which ends its forwarder), then close the link so anything
    // still writing fails fast rather than blocking on a peer that is gone.
    conn.pool.close();
    conn.watches
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    conn.release_all_workspaces();
    drop(workspace_sub);
    sink.retire();
    let _ = shutdown.shutdown_link();

    match outcome {
        Ok(()) => {
            log::debug!("control connection ({label}) closed by peer");
            Ok(())
        }
        Err(e) if is_disconnect(&e) => {
            log::debug!("control connection ({label}) ended: {e}");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// A peer going away is how connections normally end, not a failure to report.
fn is_disconnect(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
    )
}

/// Exchange hellos. `Ok(None)` means the versions did not match and the caller
/// should close.
///
/// The reply goes out **even on a mismatch**, which is the only reason the
/// client can say "the server speaks v2, I speak v1" instead of "the connection
/// dropped" — a `HELLO` carries no `req_id`, so there is no error reply to hang
/// the mismatch on.
fn handshake<R: Read>(
    r: &mut R,
    sink: &Sink,
    host: &dyn Host,
    services: &Services,
) -> io::Result<Option<ControlHello>> {
    let hello = match ControlClientMsg::read(r)? {
        ControlClientMsg::Hello(h) => h,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("control peer opened with {other:?} instead of HELLO"),
            ));
        }
    };

    // Advertised from what this server actually carries, not from what the
    // build can do. A client that sees `workspace-store` missing knows not to
    // spend a round trip asking, and — the case that matters — a machine
    // serving only a file tree does not claim to own workspace records it has
    // no file for.
    let mut features = vec![
        feature::CONTROL.to_string(),
        feature::HOST_RPC.to_string(),
        feature::STDIO_BRIDGE.to_string(),
    ];
    if services.workspaces.is_some() {
        features.push(feature::WORKSPACE_STORE.to_string());
    }

    sink.send(&ControlServerMsg::HelloOk(ControlHelloOk {
        control_version: CONTROL_VERSION,
        protocol_version: crate::daemon::protocol::PROTOCOL_VERSION,
        build: env!("CARGO_PKG_VERSION").to_string(),
        separator: host.separator(),
        home: home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        features,
        instance: crate::daemon::control::server_instance().to_string(),
    }))?;

    if hello.control_version != CONTROL_VERSION {
        log::warn!(
            "control peer speaks v{}, this build speaks v{CONTROL_VERSION}; closing",
            hello.control_version
        );
        return Ok(None);
    }
    Ok(Some(hello))
}

/// Serial number for connections, so the attach registry can tell two
/// connections apart even when the same client opens both.
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// The takeover, server side: claim `workspace` for this connection and
/// tell whoever held it.
///
/// The order is the whole behaviour. The store's record moves first (so a
/// concurrent `attachment()` never shows the workspace as free), the registry's
/// handles move under one lock, and only then is the displaced session told —
/// outside every lock, because writing to a peer that has stopped reading must
/// not hold up the client that just took over.
///
/// `dedicated` says the link was opened *for* this workspace (its hello named
/// it), which is what decides whether a later takeover closes it — see
/// [`Evicted::dedicated`].
///
/// Answers the hostname taken over from, or `None` when nobody held it.
fn attach_workspace(
    conn: &Arc<Conn>,
    workspace: &str,
    dedicated: bool,
) -> io::Result<Option<String>> {
    let store = conn.workspaces()?;
    let (displaced, evicted) = {
        // Both tables move under one lock. Held only across the two moves —
        // the notice below goes out with nothing held, because writing to a
        // peer that has stopped reading must not hold up the next client's
        // attach.
        let _handover = conn.attachments.handover();
        let displaced = store.attach(
            workspace,
            Attachment::new(conn.holder.token.clone(), conn.holder.hostname.clone()),
        );
        let evicted = conn
            .attachments
            .claim(workspace, conn.id, &conn.holder, dedicated);
        (displaced, evicted)
    };

    if let Some(evicted) = evicted {
        log::info!(
            "workspace {workspace} taken over by {} from {}",
            conn.holder.hostname,
            evicted.hostname
        );
        let notice = ControlEvent::Preempted {
            workspace: workspace.to_string(),
            by: conn.holder.hostname.clone(),
        };
        if let Err(e) = evicted.sink.send(&ControlServerMsg::Event(notice)) {
            // The displaced client is already gone. Nothing to tell, and
            // certainly not a reason to fail the attach that replaced it.
            log::debug!("could not tell the displaced session about {workspace}: {e}");
        }
        if evicted.dedicated {
            let _ = evicted.shutdown.shutdown_link();
        }
    }

    // A session re-attaching to something it already held is not a takeover, so
    // its own record is not reported back to it as one.
    Ok(displaced
        .filter(|a| a.token != conn.holder.token)
        .map(|a| a.hostname))
}

/// Release `workspace` if this connection still holds it.
///
/// Token-checked in the store *and* connection-checked in the registry, which
/// are the same guard seen from both halves: a session that was preempted and
/// then tidied up must not evict the client that took over from it.
fn detach_workspace(conn: &Arc<Conn>, workspace: &str) -> io::Result<bool> {
    let store = conn.workspaces()?;
    let _handover = conn.attachments.handover();
    let released = conn.attachments.release(workspace, conn.id);
    let forgotten = store.detach(workspace, &conn.holder.token);
    Ok(released || forgotten)
}

/// This machine's home directory, for the handshake's `home` field — the value
/// "new workspace defaults to `~`" resolves against, which has to be the
/// *server's* home and not the client's.
fn home_dir() -> Option<PathBuf> {
    let pick = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    pick("HOME").or_else(|| pick("USERPROFILE"))
}

/// Read frames until the peer stops.
fn read_loop<R: Read>(r: &mut R, conn: &Arc<Conn>) -> io::Result<()> {
    loop {
        let msg = ControlClientMsg::read(r)?;
        match msg {
            // A second hello on a live connection is a desync, not a
            // re-handshake: the peer and this side no longer agree on where the
            // stream is, and continuing would misread every later frame.
            ControlClientMsg::Hello(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "control peer sent a second HELLO on an established connection",
                ));
            }
            ControlClientMsg::Request { req_id, req } => submit(conn, req_id, req, Vec::new()),
            ControlClientMsg::RequestBlob { req_id, req, blob } => submit(conn, req_id, req, blob),
            ControlClientMsg::Cancel { req_id } => conn.cancel(req_id),
        }
    }
}

/// Queue one request onto the pool, or answer it immediately if the pool is
/// saturated.
fn submit(conn: &Arc<Conn>, req_id: u64, req: ControlRequest, blob: Vec<u8>) {
    conn.inflight
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(req_id, false);

    let job_conn = Arc::clone(conn);
    let queued = conn
        .pool
        .submit(move || run_job(&job_conn, req_id, req, blob));
    if !queued {
        conn.finish(
            req_id,
            ControlReply::Err(WireError::new(
                WireErrorKind::Other,
                "the control server has more requests queued than it will hold",
            )),
            Vec::new(),
            false,
        );
    }
}

/// Run one request on a pool worker and send its reply.
fn run_job(conn: &Arc<Conn>, req_id: u64, req: ControlRequest, blob: Vec<u8>) {
    // Cheap pre-check: a request cancelled before a worker picked it up is not
    // worth running at all. (Once it *has* started there is nothing to do — a
    // filesystem call is not interruptible, so "best effort, not
    // guaranteed" is discharged by discarding the result.)
    if conn.is_cancelled(req_id) {
        conn.forget(req_id);
        return;
    }

    let wants_blob = req.returns_blob();
    let (reply, out_blob) = match run_request(conn, req_id, req, blob) {
        Ok((ok, bytes)) => (ControlReply::Ok(ok), bytes),
        Err(e) => (ControlReply::Err(WireError::from_io(&e)), Vec::new()),
    };
    conn.finish(req_id, reply, out_blob, wants_blob);
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Drop the hits whose paths this wire cannot carry.
///
/// `SearchHit::path` is the one `PathBuf` that crosses the control dialect —
/// every path in `ControlRequest` is a `String` for exactly this reason — and
/// `serde` refuses to serialize a `Path` that is not UTF-8 rather than
/// converting it lossily. So one Latin-1 filename under the search roots makes
/// the *whole* reply unencodable: before this, the client saw no reply at all
/// and waited out its 20-second search deadline, on that keystroke and on every
/// one after it.
///
/// Dropped rather than converted lossily, because the lossy form is a path that
/// does not open — the client would be offering the user a hit it cannot act
/// on. `LocalHost` keeps full fidelity; this is the wire's limit, applied at the
/// wire.
fn drop_unsendable_hits(hits: &mut Vec<SearchHit>) {
    let before = hits.len();
    hits.retain(|hit| hit.path.to_str().is_some());
    if hits.len() != before {
        log::debug!(
            "search dropped {} hit(s) whose paths are not UTF-8",
            before - hits.len()
        );
    }
}

/// One request against the host. `Ok` carries the reply value and, for the
/// methods that have one, the bulk bytes that ride the frame's blob.
fn run_request(
    conn: &Arc<Conn>,
    req_id: u64,
    req: ControlRequest,
    blob: Vec<u8>,
) -> io::Result<(ReplyOk, Vec<u8>)> {
    let h: &dyn Host = &*conn.host;
    let p = |s: &str| PathBuf::from(s);

    Ok(match req {
        ControlRequest::Ping => (ReplyOk::Pong, Vec::new()),

        // ----- filesystem reads --------------------------------------------
        ControlRequest::ReadDir { dir, root } => {
            let root = root.map(|r| p(&r));
            let entries = h.read_dir(&p(&dir), root.as_deref())?;
            (ReplyOk::Entries(entries), Vec::new())
        }
        ControlRequest::Stat { path } => (ReplyOk::Meta(h.stat(&p(&path))?), Vec::new()),
        ControlRequest::Exists { path } => (ReplyOk::Bool(h.exists(&p(&path))), Vec::new()),
        ControlRequest::Canonicalize { path } => {
            let canon = h.canonicalize(&p(&path))?;
            (
                ReplyOk::Path(canon.to_string_lossy().into_owned()),
                Vec::new(),
            )
        }
        ControlRequest::ReadFile { path, max_bytes } => {
            let path = p(&path);
            // `max_bytes` is enforced inside `read_file`, *before* the bytes are
            // read — which is the whole reason the limit crosses the wire at all
            // rather than being applied on arrival.
            let bytes = h.read_file(&path, max_bytes)?;
            let meta = h.stat(&path)?;
            (ReplyOk::FileMeta { meta }, bytes)
        }
        ControlRequest::Search {
            roots,
            query,
            limit,
            max_dirs,
            show_hidden,
        } => {
            let roots: Vec<PathBuf> = roots.iter().map(|r| p(r)).collect();
            let mut hits = h.search(
                &roots,
                &query,
                clamp_usize(limit),
                clamp_usize(max_dirs),
                show_hidden,
            )?;
            drop_unsendable_hits(&mut hits);
            (ReplyOk::Hits(hits), Vec::new())
        }

        // ----- filesystem writes -------------------------------------------
        ControlRequest::WriteFile { path } => {
            // The post-write metadata rides back on the same round trip: the
            // editor needs that mtime to recognize its own change when the
            // watcher reports it, and a follow-up `stat` would both cost a trip
            // and leave a window for an external edit to be recorded as ours.
            // It comes from `write_file` itself now, so even that window —
            // between this handler's write and its own stat — is gone.
            (ReplyOk::Meta(h.write_file(&p(&path), &blob)?), Vec::new())
        }
        ControlRequest::CreateFileNew { path } => {
            h.create_file_new(&p(&path))?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::CreateDir { path, recursive } => {
            h.create_dir(&p(&path), recursive)?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::Rename { from, to } => {
            h.rename(&p(&from), &p(&to))?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::Remove { path, recursive } => {
            h.remove(&p(&path), recursive)?;
            (ReplyOk::Unit, Vec::new())
        }

        // ----- git -----------------------------------------------------------
        ControlRequest::RepoRoot { path } => {
            let root = h.repo_root(&p(&path))?;
            (
                ReplyOk::OptPath(root.map(|r| r.to_string_lossy().into_owned())),
                Vec::new(),
            )
        }
        ControlRequest::Git { cwd, args } => {
            let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
            // A non-zero exit is `Ok` with the status inside, never an `Err`.
            // Collapsing the two here would break `git_status`'s `Option`
            // semantics for every remote workspace at once.
            (ReplyOk::Output(h.git(&p(&cwd), &borrowed)?), Vec::new())
        }

        // ----- machine inventory ---------------------------------------------
        ControlRequest::Shells => (ReplyOk::Shells(h.shells()?), Vec::new()),

        // ----- watch ---------------------------------------------------------
        ControlRequest::WatchOpen { dirs } => {
            let id = conn.open_watch(req_id, &paths(&dirs))?;
            (ReplyOk::WatchId(id), Vec::new())
        }
        ControlRequest::WatchSet { id, dirs } => {
            conn.set_watch_dirs(id, &paths(&dirs))?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::WatchClose { id } => {
            conn.close_watch(id);
            (ReplyOk::Unit, Vec::new())
        }

        // ----- workspace store -----------------------------------------------
        // Records cross as opaque JSON: the client owns the schema, and a
        // server that parsed them would drop any field it was too old to know
        // about on the next write. See `core::workspace_store`.
        ControlRequest::WorkspaceList => (
            ReplyOk::Json(serde_json::Value::Array(conn.workspaces()?.list())),
            Vec::new(),
        ),
        ControlRequest::WorkspaceGet { id } => {
            // `NotFound` rather than a `null` payload: "there is no such
            // workspace" and "there is one and it is empty" are different
            // answers, and a client that conflated them would helpfully
            // overwrite a record it failed to read.
            let record = conn.workspaces()?.get(&id).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no workspace {id} on this machine"),
                )
            })?;
            (ReplyOk::Json(record), Vec::new())
        }
        ControlRequest::WorkspacePut { id, json } => {
            conn.workspaces()?.put(&id, json, conn.workspace_origin)?;
            (ReplyOk::Unit, Vec::new())
        }
        ControlRequest::WorkspaceDelete { id } => {
            // Deleting what is not there is success — a delete that raced
            // another client's delete has got what it asked for.
            let store = conn.workspaces()?;
            {
                // The store drops its own attachment on delete; the registry
                // has to be told, and under the same lock, or the two disagree
                // with no race needed at all. Left behind, the stale `Live`
                // entry means the *next* client to attach a workspace with this
                // id evicts a session nobody displaced — and, that entry being
                // dedicated, closes its whole link, taking every other
                // workspace on it down too.
                let _handover = conn.attachments.handover();
                store.delete(&id, conn.workspace_origin)?;
                conn.attachments.forget_workspace(&id);
            }
            (ReplyOk::Unit, Vec::new())
        }

        // ----- attachment (D8) -----------------------------------
        ControlRequest::WorkspaceAttach { id } => (
            ReplyOk::Attached {
                took_over_from: attach_workspace(conn, &id, false)?,
            },
            Vec::new(),
        ),
        ControlRequest::WorkspaceDetach { id } => {
            // Detaching something this session no longer holds is success, for
            // the same reason a redundant delete is: the caller wanted not to
            // hold it, and it does not.
            detach_workspace(conn, &id)?;
            (ReplyOk::Unit, Vec::new())
        }
    })
}

fn paths(v: &[String]) -> Vec<PathBuf> {
    v.iter().map(PathBuf::from).collect()
}

/// The wire carries `u64` where the trait takes `usize`, because a 32-bit server
/// must not silently wrap a limit into something smaller than asked for.
fn clamp_usize(v: u64) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

/// Everything one connection owns, shared by its reader thread, its pool
/// workers, and its watch forwarders.
struct Conn {
    host: SharedHost,
    sink: Arc<Sink>,
    /// Request id → cancelled. Present exactly while the request is outstanding,
    /// so a cancel for an id that has already been answered records nothing and
    /// leaks nothing.
    inflight: Mutex<HashMap<u64, bool>>,
    watches: Mutex<HashMap<u64, WatchSub>>,
    /// Watch forwarders that have been set up but must not start until their
    /// `WatchId` reply has gone out, keyed by the request that opened them.
    ///
    /// The forwarder writes to the same sink the reply does, and nothing
    /// ordered the two: a directory that changed in the instant it was first
    /// watched could push a batch that overtook the reply. The client files a
    /// watch id only once `call` returns, so it has no entry for that id yet
    /// and drops the batch under the unknown-id rule — and since the file tree
    /// relists only on a watch event, the change stays invisible until
    /// something else touches the directory.
    deferred_watches: Mutex<HashMap<u64, (u64, smol::channel::Receiver<Vec<PathBuf>>)>>,
    next_watch: AtomicU64,
    pool: Pool,
    /// The machine's workspace records, when this server serves them.
    workspaces: Option<Arc<WorkspaceStore>>,
    /// This connection's subscriber id, so its own writes do not come back to
    /// it as `WorkspaceChanged` pushes. `None` when there is no store.
    workspace_origin: Option<SubscriberId>,
    /// Shared with every other connection this server accepts — see
    /// [`AttachRegistry`].
    attachments: Arc<AttachRegistry>,
    /// This connection's serial number, which is what "the *same* connection
    /// re-attaching" is decided on.
    id: u64,
    /// Who this connection attaches as, and how to reach it.
    holder: Holder,
}

impl Conn {
    /// The workspace store, or the error a server without one answers.
    ///
    /// The message is deliberately the one the unimplemented slots gave before
    /// M5: a client talking to a file-tree-only server must get the same answer
    /// whether that server predates the store or simply was not given one.
    fn workspaces(&self) -> io::Result<&Arc<WorkspaceStore>> {
        self.workspaces
            .as_ref()
            .ok_or_else(|| io::Error::other("this server does not serve the workspace store"))
    }

    /// Give up every workspace this connection still holds, at teardown.
    ///
    /// Connection-scoped, so a workspace that was taken over from this session
    /// earlier is already gone from the registry and is not touched — the exact
    /// case the store's token check exists for, seen from the other side.
    fn release_all_workspaces(&self) {
        let _handover = self.attachments.handover();
        let released = self.attachments.release_conn(self.id);
        let Some(store) = self.workspaces.as_ref() else {
            return;
        };
        for workspace in released {
            store.detach(&workspace, &self.holder.token);
        }
    }

    fn is_cancelled(&self, req_id: u64) -> bool {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&req_id)
            .copied()
            .unwrap_or(false)
    }

    fn forget(&self, req_id: u64) {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&req_id);
    }

    /// Mark a request abandoned. Best effort by contract: work already running
    /// runs to completion, and only its reply is dropped.
    fn cancel(&self, req_id: u64) {
        let mut f = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(flag) = f.get_mut(&req_id) {
            *flag = true;
        }
    }

    /// Retire a request and, unless it was cancelled, send its reply.
    fn finish(&self, req_id: u64, reply: ControlReply, blob: Vec<u8>, wants_blob: bool) {
        let cancelled = self
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&req_id)
            .unwrap_or(false);
        if cancelled {
            log::trace!("control request {req_id} was cancelled; dropping its reply");
            self.start_deferred_watch(req_id, false);
            return;
        }

        // `wants_blob` rather than `!blob.is_empty()`: `ReadFile` on an empty
        // file still has to answer on the blob-carrying kind, because the client
        // matches on the reply shape and an empty file is a legitimate answer.
        let msg = if wants_blob && matches!(reply, ControlReply::Ok(_)) {
            ControlServerMsg::ResponseBlob {
                req_id,
                reply,
                blob,
            }
        } else {
            ControlServerMsg::Response { req_id, reply }
        };
        // Encoded before anything is written, so a reply this server cannot put
        // on the wire — a `SearchHit` whose path is not UTF-8, a `WorkspaceList`
        // grown past `MAX_FRAME` — becomes an error the client *receives*.
        // Dropping it instead leaves the client waiting out the request's whole
        // deadline (20s for a search, and again on the next keystroke) for a
        // reply that was never coming.
        let delivered = match msg.to_frame() {
            Ok((k, payload)) => match self.sink.send_frame(k, &payload) {
                Ok(()) => true,
                Err(e) => {
                    log::debug!("control reply {req_id} could not be written: {e}");
                    false
                }
            },
            Err(e) => {
                log::warn!("control reply {req_id} could not be encoded: {e}");
                let excuse = ControlServerMsg::Response {
                    req_id,
                    reply: ControlReply::Err(WireError::from_io(&e)),
                };
                if let Err(e) = self.sink.send(&excuse) {
                    log::debug!("control error reply {req_id} could not be written: {e}");
                }
                false
            }
        };

        // Only now, and only if the client actually learned the id: a batch
        // that overtook this reply would be dropped by a client that has no
        // entry for it yet. See `Conn::deferred_watches`.
        self.start_deferred_watch(req_id, delivered);
    }

    /// Open a watch and start forwarding its batches as pushes.
    fn open_watch(&self, req_id: u64, dirs: &[PathBuf]) -> io::Result<u64> {
        let sub = self.host.watch(dirs)?;
        let id = self.next_watch.fetch_add(1, Ordering::Relaxed);
        // Clone the receiver before the subscription is filed away: the
        // forwarder needs it, and `WatchSub` itself stays here so that dropping
        // the entry is what unsubscribes.
        let rx = sub.events().clone();
        self.watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, sub);
        // Parked rather than started — see `deferred_watches`. `finish` starts
        // it once the reply carrying `id` is on the wire.
        self.deferred_watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(req_id, (id, rx));
        Ok(id)
    }

    /// Start the forwarder for a watch opened by `req_id`, if there was one.
    ///
    /// Called from [`Conn::finish`] after the reply has gone out, and on the
    /// paths where no reply goes out at all — a cancelled request, or one whose
    /// reply could not be written — so a parked forwarder is never left holding
    /// a receiver nobody will read.
    fn start_deferred_watch(&self, req_id: u64, deliver: bool) {
        let parked = self
            .deferred_watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&req_id);
        let Some((id, rx)) = parked else {
            return;
        };
        if !deliver {
            // The client never learned this id, so every batch would be
            // dropped. Let the subscription go with the watch entry instead.
            self.watches
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return;
        }
        spawn_watch_forwarder(id, rx, Arc::clone(&self.sink));
    }

    fn set_watch_dirs(&self, id: u64, dirs: &[PathBuf]) -> io::Result<()> {
        let watches = self.watches.lock().unwrap_or_else(|e| e.into_inner());
        let sub = watches.get(&id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("no such watch subscription {id}"),
            )
        })?;
        sub.set_dirs(dirs)
    }

    /// Closing is dropping: the `WatchSub`'s own `Drop` releases the OS watcher,
    /// which closes the batch channel, which ends the forwarder thread. Nothing
    /// else has to be told.
    fn close_watch(&self, id: u64) {
        self.watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
}

/// Subscribe this connection to the workspace store's changes, if there is one.
///
/// Two hops rather than one, and the split is the point. The store's callback
/// runs on the thread of *whichever connection made the change*, so it does
/// nothing but enqueue; the forwarder thread is what actually writes, and a
/// peer that has stopped reading stalls only its own forwarder. Calling
/// `Sink::send` straight from the callback would have one wedged client hold up
/// every other client's `WorkspacePut`.
fn subscribe_workspaces(services: &Services, sink: &Arc<Sink>) -> Option<Subscription> {
    let store = services.workspaces.as_ref()?;
    let (tx, rx) = smol::channel::bounded::<String>(WORKSPACE_EVENT_QUEUE);
    let subscription = store.subscribe(Arc::new(move |id: &str| {
        // Never blocks. A full queue means this peer is already behind on a
        // signal that only says "refetch", and the notice sitting in the queue
        // says it just as well.
        let _ = tx.try_send(id.to_string());
    }));
    spawn_workspace_forwarder(rx, Arc::clone(sink));
    Some(subscription)
}

/// Relay workspace changes to the peer as `WorkspaceChanged` pushes.
///
/// Ends on its own when the `Subscription` is dropped: that removes the closure
/// holding the sender, which closes the channel. Same shape, and the same
/// reason, as [`spawn_watch_forwarder`].
fn spawn_workspace_forwarder(rx: smol::channel::Receiver<String>, sink: Arc<Sink>) {
    let spawned = std::thread::Builder::new()
        .name("tty7-control-workspace".into())
        .spawn(move || {
            while let Ok(id) = rx.recv_blocking() {
                let event = ControlEvent::WorkspaceChanged { id };
                if sink.send(&ControlServerMsg::Event(event)).is_err() {
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("could not start the workspace-change forwarder: {e}");
    }
}

/// Relay one subscription's batches to the peer as `CONTROL_EVENT` pushes.
///
/// The host has already coalesced and deduplicated within its window, so this
/// thread does exactly two things the wire needs: renders paths as strings, and
/// converts an oversized batch into an overflow. It ends on its own when the
/// subscription is dropped — that closes the channel — so nothing has to track
/// or join it.
fn spawn_watch_forwarder(id: u64, rx: smol::channel::Receiver<Vec<PathBuf>>, sink: Arc<Sink>) {
    let spawned = std::thread::Builder::new()
        .name("tty7-control-watch".into())
        .spawn(move || {
            while let Ok(batch) = rx.recv_blocking() {
                let event = if batch.len() > WATCH_BURST_CAP {
                    // Past the cap, enumerating costs more than re-listing: the
                    // client answers an overflow by invalidating the subtree.
                    ControlEvent::WatchOverflow { id }
                } else {
                    ControlEvent::Watch {
                        id,
                        paths: batch
                            .iter()
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect(),
                    }
                };
                if sink.send(&ControlServerMsg::Event(event)).is_err() {
                    return;
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("could not start the watch forwarder for subscription {id}: {e}");
    }
}

/// The connection's write half.
///
/// One mutex, one whole frame per acquisition. Pool workers and watch forwarders
/// all write here concurrently, and a frame that interleaved with another would
/// desync the peer permanently — there is no resynchronization point in a
/// length-prefixed stream.
struct Sink {
    out: Mutex<Option<Box<dyn Write + Send>>>,
}

impl Sink {
    fn new<W: Write + Send + 'static>(w: W) -> Sink {
        Sink {
            out: Mutex::new(Some(Box::new(w))),
        }
    }

    fn send(&self, msg: &ControlServerMsg) -> io::Result<()> {
        let (k, payload) = msg.to_frame()?;
        self.send_frame(k, &payload)
    }

    /// The write half of [`Sink::send`], for callers that already encoded.
    ///
    /// Split so that "this reply cannot be encoded" and "this link is gone" are
    /// distinguishable: only the second may have put bytes on the wire, and only
    /// the first leaves the connection well enough to answer on.
    fn send_frame(&self, k: u8, payload: &[u8]) -> io::Result<()> {
        let mut slot = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let w = slot.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "control connection is closed")
        })?;
        crate::daemon::protocol::write_frame(&mut *w, k, payload).and_then(|()| w.flush())
    }

    /// Drop the write half at teardown, so a worker that finishes afterwards
    /// fails immediately instead of writing into a link nobody is reading.
    fn retire(&self) {
        *self.out.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

// ---------------------------------------------------------------------------
// The pool
// ---------------------------------------------------------------------------

type Job = Box<dyn FnOnce() + Send + 'static>;

/// An elastic worker pool: grows on demand to [`MAX_WORKERS`], shrinks by
/// letting idle workers retire after [`WORKER_LINGER`].
///
/// Deliberately not a fixed pool. A fixed `N` guarantees that `N` slow requests
/// stall the `N+1`th — which is the exact failure `req_id` multiplexing exists to
/// prevent, reintroduced one layer down. Deliberately not thread-per-request
/// either: a file tree expanding a directory issues dozens of `stat`s, and a
/// thread per `stat` costs more than the `stat`.
struct Pool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    state: Mutex<PoolState>,
    wake: Condvar,
}

struct PoolState {
    jobs: VecDeque<Job>,
    /// Live worker threads.
    workers: usize,
    /// Workers currently parked on [`PoolInner::wake`]. A job only needs a *new*
    /// worker when this is zero.
    idle: usize,
    closed: bool,
}

impl PoolState {
    /// Whether a job just queued needs a worker spawned for it.
    ///
    /// Compares the backlog against the parked workers rather than asking
    /// whether *any* worker is parked. `idle` counts a worker from before it
    /// parks until after it has re-acquired the lock on its way out, so for the
    /// whole wake-up window a worker that has already been handed a job still
    /// looks free — and the `notify_one` a second submit sends in that window
    /// goes to a thread that has left the wait set, so it is lost.
    ///
    /// Concretely: with `k` workers parked, a client pipelining `k+1` frames in
    /// one read — a `Git` plus `k` `ReadDir`s, which is what the file tree and
    /// the branch line produce together — got `k` of them running and left the
    /// last queued behind a `git status` that can take twenty seconds. That is
    /// the head-of-line blocking this pool is elastic in order to avoid.
    ///
    /// Counting both sides makes the window harmless: the job destined for a
    /// parked-but-notified worker is still in `jobs`, so it cancels that
    /// worker out and the next job over sees no spare capacity.
    fn wants_another_worker(&self) -> bool {
        self.jobs.len() > self.idle && self.workers < MAX_WORKERS
    }
}

impl Pool {
    fn new() -> Pool {
        Pool {
            inner: Arc::new(PoolInner {
                state: Mutex::new(PoolState {
                    jobs: VecDeque::new(),
                    workers: 0,
                    idle: 0,
                    closed: false,
                }),
                wake: Condvar::new(),
            }),
        }
    }

    /// Queue `job`. `false` means the pool is closed or the backlog is full and
    /// the caller must answer the request itself.
    fn submit(&self, job: impl FnOnce() + Send + 'static) -> bool {
        let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed || st.jobs.len() >= MAX_QUEUED {
            return false;
        }
        st.jobs.push_back(Box::new(job));

        // Spawn only when the backlog outruns the workers parked to take it, so
        // a steady stream of requests is served by one warm worker rather than a
        // thread per call.
        if st.wants_another_worker() {
            st.workers += 1;
            let inner = Arc::clone(&self.inner);
            match std::thread::Builder::new()
                .name("tty7-control-worker".into())
                .spawn(move || worker(inner))
            {
                Ok(_) => return true,
                Err(e) => {
                    // Out of threads: keep the job queued for whoever is already
                    // running, and only fail if there is nobody at all.
                    st.workers -= 1;
                    log::warn!("could not start a control worker: {e}");
                    if st.workers == 0 {
                        st.jobs.pop_back();
                        return false;
                    }
                }
            }
        }
        drop(st);
        self.inner.wake.notify_one();
        true
    }

    /// Stop the pool and drop anything still queued.
    ///
    /// Clearing the queue is not just tidiness: a queued job holds an `Arc<Conn>`
    /// and the `Conn` holds the pool, so leaving jobs behind would be a reference
    /// cycle that keeps the whole connection — watches included — alive forever.
    ///
    /// Running workers are not joined. One may be twenty seconds into a `git`,
    /// and the caller of `close` is a connection teardown that must not wait on
    /// it; the worker finds a retired sink, fails its write, and exits.
    fn close(&self) {
        {
            let mut st = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            st.closed = true;
            st.jobs.clear();
        }
        self.inner.wake.notify_all();
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.close();
    }
}

fn worker(inner: Arc<PoolInner>) {
    loop {
        let job = {
            let mut st = inner.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = st.jobs.pop_front() {
                    break job;
                }
                if st.closed {
                    st.workers -= 1;
                    return;
                }
                st.idle += 1;
                let (guard, timeout) = inner
                    .wake
                    .wait_timeout(st, WORKER_LINGER)
                    .unwrap_or_else(|e| e.into_inner());
                st = guard;
                st.idle -= 1;
                if timeout.timed_out() && st.jobs.is_empty() {
                    st.workers -= 1;
                    return;
                }
            }
        };
        job();
    }
}

// ---------------------------------------------------------------------------
// The machine-local control socket
// ---------------------------------------------------------------------------

/// Overrides [`control_socket_path`]. Set by anything that needs its own
/// endpoint instead of the per-user one: the end-to-end tests, and a second
/// server on a shared box.
pub const CONTROL_SOCK_ENV: &str = "TTY7_CONTROL_SOCK";

#[cfg(unix)]
mod sock {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::{UnixListener, UnixStream};

    /// `sockaddr_un.sun_path` is 104 bytes on macOS and 108 on Linux, NUL
    /// included. Staying under the smaller figure keeps one code path.
    const MAX_SOCKET_PATH_BYTES: usize = 100;

    /// Where a `tty7-server` listens for control connections.
    ///
    /// | Order | Path | Why |
    /// |---|---|---|
    /// | 1 | `$TTY7_CONTROL_SOCK` | Explicit wins; this is how tests and a second instance get their own endpoint |
    /// | 2 | `$XDG_RUNTIME_DIR/tty7/daemon.sock` | The per-user, per-boot, already-0700 directory Linux provides for exactly this |
    /// | 3 | `~/.local/share/tty7/daemon.sock` | No `XDG_RUNTIME_DIR` (macOS, minimal containers) |
    /// | 4 | `<runtime-or-tmp>/tty7-<hash>.sock` | Any of the above too long for `sun_path` |
    ///
    /// The hashed fallback is the same construction
    /// [`daemon::transport`](crate::daemon::transport) uses for the pane socket,
    /// over the same [`fnv1a64`](crate::host::fnv1a64) — deliberately a fixed
    /// hash rather than `DefaultHasher`, because the two processes that have to
    /// derive the same path can be different builds.
    pub fn control_socket_path() -> io::Result<PathBuf> {
        if let Some(explicit) = std::env::var_os(CONTROL_SOCK_ENV).filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(explicit));
        }

        let dir = runtime_dir()
            .map(|d| d.join("tty7"))
            .or_else(|| home_dir().map(|h| h.join(".local").join("share").join("tty7")))
            .ok_or_else(|| {
                io::Error::other(
                    "no $XDG_RUNTIME_DIR and no home directory to place the control socket in",
                )
            })?;

        let fallbacks: Vec<PathBuf> = [runtime_dir(), Some(std::env::temp_dir())]
            .into_iter()
            .flatten()
            .collect();
        socket_path_in(&dir, &fallbacks)
    }

    /// The length-aware half of [`control_socket_path`], split out because the
    /// whole of it reads `$XDG_RUNTIME_DIR` — and a test that set that would be
    /// changing a process-global under every other test running beside it.
    pub(super) fn socket_path_in(dir: &Path, fallbacks: &[PathBuf]) -> io::Result<PathBuf> {
        let inline = dir.join("daemon.sock");
        if fits(&inline) {
            return Ok(inline);
        }

        // The hashed name keeps distinct directories on distinct endpoints while
        // fitting in `sun_path`. Every candidate base is tried because any of
        // them can itself be too long — a deep `$XDG_RUNTIME_DIR` makes the
        // "short" path no shorter — and handing back something `bind` will
        // reject turns a fixable configuration into an unexplained failure.
        use std::os::unix::ffi::OsStrExt as _;
        let name = format!(
            "tty7-{:016x}.sock",
            crate::host::fnv1a64(dir.as_os_str().as_bytes())
        );
        for base in fallbacks {
            let candidate = base.join(&name);
            if fits(&candidate) {
                return Ok(candidate);
            }
        }
        Err(io::Error::other(format!(
            "no directory short enough for a control socket ({MAX_SOCKET_PATH_BYTES}-byte \
             sun_path limit); set {CONTROL_SOCK_ENV} to one that is"
        )))
    }

    fn runtime_dir() -> Option<PathBuf> {
        std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
    }

    fn fits(p: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt as _;
        p.as_os_str().as_bytes().len() <= MAX_SOCKET_PATH_BYTES
    }

    /// Bind the control socket, replacing a stale one.
    ///
    /// Permissions are the access boundary — a Unix socket has no other, and
    /// what is behind it is `ReadFile`/`WriteFile`/`Git` on arbitrary paths as
    /// this user. So the socket has to be 0600 from the first instant its final
    /// name exists, which `bind` alone cannot give: `bind` creates the node at
    /// `0777 & ~umask`, and a `chmod` on the next line is a window. Under a
    /// `umask 002` — the default wherever user-private groups are configured —
    /// that window is group-connectable.
    ///
    /// So the umask is tightened across the `bind` itself. Not a staging
    /// directory and a rename, which would also close the window: the staging
    /// path is longer than the final one, and `sun_path` is the one budget here
    /// with no room to spend — [`socket_path_in`] already falls back to a hashed
    /// name to stay under it.
    pub fn bind_control_socket(path: &Path) -> io::Result<UnixListener> {
        let parent = path.parent().unwrap_or(Path::new("."));
        // `create_dir_all` and *only then* a chmod, on a directory that did not
        // exist a moment ago: `path` can be `$TTY7_CONTROL_SOCK` or the hashed
        // fallback, whose parent is `$XDG_RUNTIME_DIR` or `/tmp` — directories
        // this process does not own and must not re-permission. Tightening
        // `/tmp` to 0700 would lock every other user out of it, sticky bit and
        // all, with nothing linking the breakage back to tty7.
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
            // Ours by construction, since it did not exist above. Best effort:
            // losing the race to another tty7 starting at the same instant
            // leaves the directory correct anyway.
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }

        // A socket file left by a crashed server looks exactly like a live one.
        // Connecting is the only way to tell, so probe before clearing.
        if path.exists() {
            match UnixStream::connect(path) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!(
                            "a control server is already listening on {}",
                            path.display()
                        ),
                    ));
                }
                Err(_) => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }

        let listener = bind_private(path)?;
        // Belt and braces on two counts: it narrows the 0700 the umask below
        // yields to the 0600 a socket actually needs, and it covers a
        // filesystem that ignores the umask entirely (some FUSE mounts,
        // anything with a default ACL) — which would otherwise leave the socket
        // wide open with nothing to notice it.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    }

    /// `bind`, with the umask tightened so the node is created owner-only
    /// rather than created at `0777 & ~umask` and fixed up afterwards.
    ///
    /// `0o077`, not `0o177`: the umask is process-global for the length of the
    /// `bind`, and a *directory* another thread creates in that window would
    /// come out without its owner-execute bit and be unusable — the test suite
    /// found this the hard way. Masking only the group and other bits leaves
    /// anything created alongside it working while still giving the socket no
    /// group or world access, which is the whole property: connecting to a Unix
    /// socket needs write permission on it.
    pub(super) fn bind_private(path: &Path) -> io::Result<UnixListener> {
        // Serializes tty7's own binds against each other, so two of them cannot
        // interleave their save/restore and leave the umask tightened.
        static UMASK: Mutex<()> = Mutex::new(());
        let _held = UMASK.lock().unwrap_or_else(|e| e.into_inner());

        // SAFETY: `umask` is always successful and has no preconditions; the
        // only hazard is the process-global effect, which the lock and the
        // immediate restore below bound.
        let previous = unsafe { libc::umask(0o077) };
        let bound = UnixListener::bind(path);
        unsafe { libc::umask(previous) };
        bound
    }

    /// Serve control connections on `listener` until it fails, one thread per
    /// connection.
    pub fn serve_listener(listener: UnixListener, host: SharedHost) {
        serve_listener_with(listener, host, Services::none())
    }

    /// [`serve_listener`], with the extra services every connection gets.
    ///
    /// One [`WorkspaceStore`](crate::core::workspace_store::WorkspaceStore)
    /// shared by every connection, which is what makes a change on one visible
    /// to the others: two stores over one file would each believe their own
    /// copy and the last save would win silently.
    pub fn serve_listener_with(listener: UnixListener, host: SharedHost, services: Services) {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let host = Arc::clone(&host);
                    let services = services.clone();
                    let spawned = std::thread::Builder::new()
                        .name("tty7-control-conn".into())
                        .spawn(move || {
                            if let Err(e) = serve_with(stream, host, services) {
                                log::warn!("control connection failed: {e}");
                            }
                        });
                    if let Err(e) = spawned {
                        log::warn!("could not start a control connection thread: {e}");
                    }
                }
                // One bad accept must not take the server down; the daemon's own
                // listener has behaved this way since it was written.
                Err(e) => log::warn!("control accept failed: {e}"),
            }
        }
    }

    /// Bind the per-user control socket and serve it on a background thread.
    ///
    /// Returns the path it is listening on. Used by `tty7-server --daemon`,
    /// which then goes on to run the pane server on the *other* socket — the two
    /// dialects are independent listeners on purpose, so a machine can serve a
    /// remote workspace's files without serving panes, and vice versa.
    pub fn spawn_control_listener(host: SharedHost) -> io::Result<PathBuf> {
        spawn_control_listener_with(host, Services::none())
    }

    /// [`spawn_control_listener`], with the extra services every connection
    /// gets.
    pub fn spawn_control_listener_with(
        host: SharedHost,
        services: Services,
    ) -> io::Result<PathBuf> {
        let path = control_socket_path()?;
        let listener = bind_control_socket(&path)?;
        std::thread::Builder::new()
            .name("tty7-control-listener".into())
            .spawn(move || serve_listener_with(listener, host, services))?;
        Ok(path)
    }
}

#[cfg(unix)]
pub use sock::{
    bind_control_socket, control_socket_path, serve_listener, serve_listener_with,
    spawn_control_listener, spawn_control_listener_with,
};

/// The pool is plain threads and channels, so unlike the rest of this file's
/// tests — which need a Unix socket pair — these hold on every platform.
#[cfg(test)]
mod pool_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    /// Workers are spawned only when nobody is free, so a serial stream of jobs
    /// costs one thread rather than one thread per job.
    #[test]
    fn the_pool_reuses_a_warm_worker() {
        let pool = Pool::new();

        // A job signals from *inside* itself, so it has sent before its worker
        // has parked again. Submitting in that window is a genuine "nobody is
        // free" and legitimately spawns a second worker — so let the pool settle
        // first, and the assertion is about reuse rather than about timing.
        let settled = || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                let st = pool.inner.state.lock().unwrap();
                if st.idle == st.workers {
                    return;
                }
                drop(st);
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("the pool never went idle");
        };

        for _ in 0..50 {
            let (tx, rx) = std::sync::mpsc::channel();
            assert!(pool.submit(move || {
                let _ = tx.send(());
            }));
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
            settled();
        }
        let st = pool.inner.state.lock().unwrap();
        assert_eq!(
            st.workers, 1,
            "50 sequential jobs should not want 50 threads"
        );
    }

    /// The spawn decision, at the three states that distinguish it from
    /// "is anybody parked?".
    ///
    /// Driven directly because the state it has to get right — a worker that
    /// has been notified but has not yet re-acquired the lock, so it is counted
    /// in `idle` while the job meant for it is still counted in `jobs` — is a
    /// few instructions wide and a racing test passes against the broken rule
    /// far more often than it fails.
    #[test]
    fn a_worker_is_spawned_when_the_backlog_outruns_the_parked_workers() {
        let state = |jobs: usize, workers: usize, idle: usize| PoolState {
            jobs: (0..jobs).map(|_| Box::new(|| ()) as Job).collect(),
            workers,
            idle,
            closed: false,
        };

        // One parked worker, one job: it is exactly the warm-worker case, and
        // spawning here is what would cost a thread per request.
        assert!(!state(1, 1, 1).wants_another_worker());

        // One parked worker, two jobs. The second job arrived inside the
        // first's wake-up window, so `idle` still says 1 — and the old rule,
        // which only asked whether `idle == 0`, left this job queued behind a
        // request that can take twenty seconds.
        assert!(state(2, 1, 1).wants_another_worker());

        // Nobody parked at all.
        assert!(state(1, 1, 0).wants_another_worker());

        // And the ceiling still holds.
        assert!(!state(64, MAX_WORKERS, 0).wants_another_worker());
    }

    /// And it does grow when work genuinely overlaps.
    #[test]
    fn the_pool_grows_for_concurrent_work() {
        let pool = Pool::new();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        for _ in 0..8 {
            let gate = Arc::clone(&gate);
            let done_tx = done_tx.clone();
            assert!(pool.submit(move || {
                let (lock, cv) = &*gate;
                let mut open = lock.lock().unwrap();
                let _ = done_tx.send(());
                while !*open {
                    open = cv.wait(open).unwrap();
                }
            }));
        }
        for _ in 0..8 {
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert_eq!(pool.inner.state.lock().unwrap().workers, 8);

        let (lock, cv) = &*gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
        pool.close();
    }

    /// Closing drops the backlog. That is what breaks the `Conn` → `Pool` → job
    /// → `Arc<Conn>` cycle; leaving the jobs queued would keep every watch on a
    /// dead connection alive for the life of the process.
    #[test]
    fn closing_the_pool_drops_queued_work() {
        let pool = Pool::new();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let ran = Arc::new(AtomicBool::new(false));

        // One job to occupy the single worker...
        let blocker = Arc::clone(&gate);
        assert!(pool.submit(move || {
            let (lock, cv) = &*blocker;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
        }));
        std::thread::sleep(Duration::from_millis(100));
        // ...and one that must never run.
        let ran2 = Arc::clone(&ran);
        pool.submit(move || ran2.store(true, Ordering::SeqCst));

        pool.close();
        let (lock, cv) = &*gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !ran.load(Ordering::SeqCst),
            "a job queued at close still ran"
        );
        assert!(!pool.submit(|| {}), "a closed pool must refuse work");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::daemon::control::{ControlHello, MTime, feature};
    use crate::host::local::LocalHost;
    use crate::host::remote::RemoteHost;
    use crate::host::{Entry, HostId, Meta, Output};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    /// A server on one end of a socket pair and a `RemoteHost` on the other.
    ///
    /// The same shape as the real thing minus the process boundary, which keeps
    /// these unit tests to milliseconds while the cross-process proof lives in
    /// `tty7-server`'s stdio integration test.
    struct Pair {
        host: Arc<RemoteHost>,
    }

    fn pair_with(server_host: SharedHost) -> Pair {
        let (server, client) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let _ = serve(server, server_host);
        });
        let hello = ControlHello::host_rpc("test-token", "test-host");
        let host = RemoteHost::over_unix(client, "test:pair", &hello).unwrap();
        Pair { host }
    }

    fn pair() -> Pair {
        pair_with(LocalHost::new())
    }

    /// A raw client: the frames, with no `Host` in the way. Some assertions are
    /// about the wire itself (which kind a reply used, whether a blob rode
    /// along) and cannot be made through a typed API that hides it.
    fn raw() -> (UnixStream, ControlHelloOk) {
        raw_with(Services::none())
    }

    /// [`raw`], against a server offering `services`.
    fn raw_with(services: Services) -> (UnixStream, ControlHelloOk) {
        raw_hello(services, ControlHello::host_rpc("t", "h")).0
    }

    /// [`raw_with`], saying a specific hello — the token, hostname and bound
    /// workspace are what the takeover decides on, so the attach tests need to
    /// choose them. Also answers the server thread's handle so a test can prove
    /// the connection was closed rather than merely quiet.
    fn raw_hello(
        services: Services,
        hello: ControlHello,
    ) -> ((UnixStream, ControlHelloOk), std::thread::JoinHandle<()>) {
        let (server, mut client) = UnixStream::pair().unwrap();
        let served = std::thread::spawn(move || {
            let _ = serve_with(server, LocalHost::new(), services);
        });
        ControlClientMsg::Hello(hello).encode(&mut client).unwrap();
        client.flush().unwrap();
        let ok = match ControlServerMsg::read(&mut client).unwrap() {
            ControlServerMsg::HelloOk(ok) => ok,
            other => panic!("{other:?}"),
        };
        ((client, ok), served)
    }

    /// A hello that binds a connection to one workspace.
    fn hello_for(workspace: &str, token: &str, hostname: &str) -> ControlHello {
        ControlHello {
            control_version: CONTROL_VERSION,
            workspace: Some(workspace.to_string()),
            client_token: token.to_string(),
            client_hostname: hostname.to_string(),
        }
    }

    /// Issue one request on a raw socket and return its reply, collecting any
    /// pushes that arrive first — a `Preempted` and a reply race by
    /// construction, so a reader that assumed the next frame was its answer
    /// would be flaky rather than wrong.
    fn round_trip(
        sock: &mut UnixStream,
        req_id: u64,
        req: ControlRequest,
    ) -> (ControlReply, Vec<ControlEvent>) {
        ControlClientMsg::Request { req_id, req }
            .encode(sock)
            .unwrap();
        sock.flush().unwrap();
        let mut events = Vec::new();
        loop {
            match ControlServerMsg::read(sock).unwrap() {
                ControlServerMsg::Response { req_id: got, reply } if got == req_id => {
                    return (reply, events);
                }
                ControlServerMsg::Event(e) => events.push(e),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    /// Read frames until a `Preempted` push arrives, or the link ends.
    fn await_preempted(sock: &mut UnixStream) -> Option<ControlEvent> {
        loop {
            match ControlServerMsg::read(sock) {
                Ok(ControlServerMsg::Event(e @ ControlEvent::Preempted { .. })) => return Some(e),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    /// Wait for the hello-time attach to land. It runs on the server thread
    /// *after* the handshake reply goes out, so a client that read `HELLO_OK`
    /// has not necessarily been recorded yet.
    fn await_holder(registry: &AttachRegistry, workspace: &str, hostname: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while registry.holder(workspace).map(|(_, h)| h).as_deref() != Some(hostname) {
            assert!(
                Instant::now() < deadline,
                "{workspace} is held by {:?}, not {hostname}",
                registry.holder(workspace)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn workspace_services() -> (Services, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = WorkspaceStore::open(dir.path().join("workspaces.json"));
        (Services::with_workspaces(store), dir)
    }

    // -----------------------------------------------------------------------
    // Concurrency
    // -----------------------------------------------------------------------

    /// A host whose `git` sleeps, and whose every other method is the real one.
    ///
    /// Delegation rather than a stub because the slow request has to be a
    /// *real* request that really occupies a worker; a stub host would prove the
    /// pool schedules closures, not that the server stays responsive.
    struct SlowGit {
        inner: SharedHost,
        delay: Duration,
        running: Arc<AtomicBool>,
    }

    impl Host for SlowGit {
        fn id(&self) -> HostId {
            self.inner.id()
        }
        fn separator(&self) -> char {
            self.inner.separator()
        }
        fn is_absolute(&self, p: &Path) -> bool {
            self.inner.is_absolute(p)
        }
        fn read_dir(&self, dir: &Path, root: Option<&Path>) -> io::Result<Vec<Entry>> {
            self.inner.read_dir(dir, root)
        }
        fn stat(&self, p: &Path) -> io::Result<Meta> {
            self.inner.stat(p)
        }
        fn read_file(&self, p: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
            self.inner.read_file(p, max_bytes)
        }
        fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
            self.inner.canonicalize(p)
        }
        fn search(
            &self,
            roots: &[PathBuf],
            query: &str,
            limit: usize,
            max_dirs: usize,
            show_hidden: bool,
        ) -> io::Result<Vec<SearchHit>> {
            self.inner
                .search(roots, query, limit, max_dirs, show_hidden)
        }
        fn write_file(&self, p: &Path, bytes: &[u8]) -> io::Result<Meta> {
            self.inner.write_file(p, bytes)
        }
        fn create_file_new(&self, p: &Path) -> io::Result<()> {
            self.inner.create_file_new(p)
        }
        fn create_dir(&self, p: &Path, recursive: bool) -> io::Result<()> {
            self.inner.create_dir(p, recursive)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }
        fn remove(&self, p: &Path, recursive: bool) -> io::Result<()> {
            self.inner.remove(p, recursive)
        }
        fn repo_root(&self, p: &Path) -> io::Result<Option<PathBuf>> {
            self.inner.repo_root(p)
        }
        fn git(&self, _cwd: &Path, _args: &[&str]) -> io::Result<Output> {
            self.running.store(true, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            Ok(Output {
                status: Some(0),
                stdout: b"slow".to_vec(),
                stderr: Vec::new(),
            })
        }
        fn shells(&self) -> io::Result<crate::host::ShellInventory> {
            self.inner.shells()
        }
        fn watch(&self, dirs: &[PathBuf]) -> io::Result<WatchSub> {
            self.inner.watch(dirs)
        }
    }

    /// **The property the whole design is for.** A twenty-second `git` must not
    /// hold up a five-millisecond `stat`, which means the server has to be
    /// genuinely concurrent — a serial loop passes every other test in this file
    /// and fails this one.
    #[test]
    fn a_slow_request_does_not_block_a_fast_one() {
        let running = Arc::new(AtomicBool::new(false));
        let p = pair_with(Arc::new(SlowGit {
            inner: LocalHost::new(),
            delay: Duration::from_millis(1500),
            running: Arc::clone(&running),
        }));

        let tmp = tempfile::TempDir::new().unwrap();
        let slow_host = Arc::clone(&p.host);
        let dir = tmp.path().to_path_buf();
        let slow = std::thread::spawn(move || slow_host.git(&dir, &["status"]));

        // Wait for the slow request to actually be inside the handler, so the
        // measurement below is about overlap and not about scheduling luck.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !running.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(running.load(Ordering::SeqCst), "the slow request never ran");

        let started = Instant::now();
        p.host.stat(tmp.path()).unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(600),
            "a stat behind an in-flight git took {elapsed:?} — the server is serializing"
        );

        let out = slow.join().unwrap().unwrap();
        assert_eq!(out.stdout, b"slow");
    }

    /// And it holds at scale: dozens of parked requests still leave the pool
    /// able to answer, which a fixed-size pool would not.
    #[test]
    fn many_slow_requests_still_leave_the_server_answering() {
        let running = Arc::new(AtomicBool::new(false));
        let p = pair_with(Arc::new(SlowGit {
            inner: LocalHost::new(),
            delay: Duration::from_millis(1200),
            running: Arc::clone(&running),
        }));
        let tmp = tempfile::TempDir::new().unwrap();

        let mut slow = Vec::new();
        for _ in 0..24 {
            let host = Arc::clone(&p.host);
            let dir = tmp.path().to_path_buf();
            slow.push(std::thread::spawn(move || {
                let _ = host.git(&dir, &["status"]);
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !running.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }

        let started = Instant::now();
        assert!(p.host.exists(tmp.path()));
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "24 in-flight gits stalled a trivial request"
        );
        for t in slow {
            let _ = t.join();
        }
    }

    /// Replies are matched by id, so they may arrive in any order — and here
    /// they arrive in the opposite one.
    #[test]
    fn replies_come_back_out_of_order() {
        let running = Arc::new(AtomicBool::new(false));
        let p = pair_with(Arc::new(SlowGit {
            inner: LocalHost::new(),
            delay: Duration::from_millis(800),
            running: Arc::clone(&running),
        }));
        let tmp = tempfile::TempDir::new().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let host = Arc::clone(&p.host);
        let dir = tmp.path().to_path_buf();
        let slow_tx = tx.clone();
        let slow = std::thread::spawn(move || {
            let _ = host.git(&dir, &["status"]);
            let _ = slow_tx.send("git");
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !running.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        p.host.stat(tmp.path()).unwrap();
        tx.send("stat").unwrap();

        assert_eq!(
            rx.recv().unwrap(),
            "stat",
            "the later request answered first"
        );
        assert_eq!(rx.recv().unwrap(), "git");
        slow.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // Handshake
    // -----------------------------------------------------------------------

    /// The handshake reports this machine, and advertises what it can actually
    /// do — the client reads `separator` to build every path it will ever send.
    #[test]
    fn the_handshake_describes_this_server() {
        let p = pair();
        let peer = p.host.peer();
        assert_eq!(peer.control_version, CONTROL_VERSION);
        assert_eq!(
            peer.protocol_version,
            crate::daemon::protocol::PROTOCOL_VERSION
        );
        assert_eq!(peer.separator, std::path::MAIN_SEPARATOR);
        assert!(peer.has_feature(feature::CONTROL));
        assert!(peer.has_feature(feature::HOST_RPC));
        assert!(
            !peer.home.is_empty(),
            "the server's $HOME backs `new workspace in ~`"
        );
    }

    /// A version mismatch is answered, then closed. Answering first is what lets
    /// the client say *which* version it met instead of "the link dropped".
    #[test]
    fn a_version_mismatch_is_answered_then_closed() {
        let (server, mut client) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let _ = serve(server, LocalHost::new());
        });

        let mut hello = ControlHello::host_rpc("t", "h");
        hello.control_version = CONTROL_VERSION + 7;
        ControlClientMsg::Hello(hello).encode(&mut client).unwrap();
        client.flush().unwrap();

        match ControlServerMsg::read(&mut client).unwrap() {
            ControlServerMsg::HelloOk(ok) => assert_eq!(ok.control_version, CONTROL_VERSION),
            other => panic!("{other:?}"),
        }
        // And then nothing: the link is closed, not merely idle.
        let mut buf = [0u8; 1];
        assert_eq!(
            client.read(&mut buf).unwrap(),
            0,
            "the server should hang up"
        );
    }

    /// The client's own connect path refuses a peer that speaks another version,
    /// naming both — the end-to-end version of the case above.
    #[test]
    fn the_client_refuses_a_mismatched_peer() {
        let (server, client) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            // A hand-rolled server that claims a different dialect.
            let mut s = server;
            let _ = ControlClientMsg::read(&mut s);
            let _ = ControlServerMsg::HelloOk(ControlHelloOk {
                control_version: CONTROL_VERSION + 1,
                protocol_version: 3,
                build: "other".into(),
                separator: '/',
                home: "/root".into(),
                features: vec![],
                instance: "other-instance".into(),
            })
            .encode(&mut s);
        });
        let hello = ControlHello::host_rpc("t", "h");
        let err = RemoteHost::over_unix(client, "test:skew", &hello).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported, "{err}");
    }

    // -----------------------------------------------------------------------
    // Wire-level guarantees
    // -----------------------------------------------------------------------

    /// `max_bytes` is refused **before the bytes move**. The whole reason the
    /// limit crosses the wire is to avoid a transatlantic transfer that ends in
    /// the client throwing the result away — so the failing reply must be a
    /// plain response with no blob at all.
    #[test]
    fn an_oversized_read_ships_no_bytes() {
        let (mut client, _) = raw();
        let tmp = tempfile::TempDir::new().unwrap();
        let fat = tmp.path().join("fat.bin");
        std::fs::write(&fat, vec![b'x'; 4096]).unwrap();

        ControlClientMsg::Request {
            req_id: 1,
            req: ControlRequest::ReadFile {
                path: fat.to_string_lossy().into_owned(),
                max_bytes: 16,
            },
        }
        .encode(&mut client)
        .unwrap();
        client.flush().unwrap();

        match ControlServerMsg::read(&mut client).unwrap() {
            ControlServerMsg::Response { req_id, reply } => {
                assert_eq!(req_id, 1);
                match reply {
                    ControlReply::Err(e) => assert_eq!(e.kind, WireErrorKind::FileTooLarge),
                    other => panic!("{other:?}"),
                }
            }
            ControlServerMsg::ResponseBlob { blob, .. } => {
                panic!("the refusal carried {} bytes of the file", blob.len())
            }
            other => panic!("{other:?}"),
        }
    }

    /// An empty file still answers on the blob-carrying kind: the client matches
    /// on the reply's *shape*, and "zero bytes" is a real answer rather than an
    /// absent one.
    #[test]
    fn an_empty_read_still_uses_the_blob_kind() {
        let (mut client, _) = raw();
        let tmp = tempfile::TempDir::new().unwrap();
        let empty = tmp.path().join("empty.txt");
        std::fs::write(&empty, b"").unwrap();

        ControlClientMsg::Request {
            req_id: 1,
            req: ControlRequest::ReadFile {
                path: empty.to_string_lossy().into_owned(),
                max_bytes: 1024,
            },
        }
        .encode(&mut client)
        .unwrap();
        client.flush().unwrap();

        match ControlServerMsg::read(&mut client).unwrap() {
            ControlServerMsg::ResponseBlob { reply, blob, .. } => {
                assert!(blob.is_empty());
                assert!(matches!(reply, ControlReply::Ok(ReplyOk::FileMeta { .. })));
            }
            other => panic!("{other:?}"),
        }
    }

    /// Every error class the server can produce survives the round trip as
    /// itself. The client rebuilds an `io::Error` from the wire kind, and a
    /// mapping that was not a bijection would silently turn, say, a
    /// `DirectoryNotEmpty` into an `Other` and break the file tree's delete
    /// confirmation.
    #[test]
    fn error_kinds_survive_the_round_trip() {
        let p = pair();
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();

        let cases: Vec<(io::ErrorKind, io::Error)> = vec![
            (
                io::ErrorKind::NotFound,
                p.host.stat(&root.join("nope")).unwrap_err(),
            ),
            (io::ErrorKind::AlreadyExists, {
                let f = root.join("taken.txt");
                p.host.write_file(&f, b"x").unwrap();
                p.host.create_file_new(&f).unwrap_err()
            }),
            (io::ErrorKind::FileTooLarge, {
                let f = root.join("fat.txt");
                p.host.write_file(&f, &vec![b'x'; 512]).unwrap();
                p.host.read_file(&f, 16).unwrap_err()
            }),
            (io::ErrorKind::IsADirectory, {
                let d = root.join("adir");
                p.host.create_dir(&d, false).unwrap();
                p.host.read_file(&d, 1024).unwrap_err()
            }),
            (io::ErrorKind::DirectoryNotEmpty, {
                let d = root.join("full");
                p.host.create_dir(&d, false).unwrap();
                p.host.write_file(&d.join("f"), b"x").unwrap();
                p.host.remove(&d, false).unwrap_err()
            }),
        ];
        for (want, got) in cases {
            assert_eq!(got.kind(), want, "{got}");
            // And the kind is what the wire carried, not a local guess: the
            // message came from the server.
            assert!(!got.to_string().is_empty());
        }

        // The mapping itself, in both directions, over every class.
        for kind in [
            WireErrorKind::NotFound,
            WireErrorKind::PermissionDenied,
            WireErrorKind::AlreadyExists,
            WireErrorKind::InvalidInput,
            WireErrorKind::NotADirectory,
            WireErrorKind::IsADirectory,
            WireErrorKind::DirectoryNotEmpty,
            WireErrorKind::FileTooLarge,
            WireErrorKind::TimedOut,
            WireErrorKind::ConnectionReset,
            WireErrorKind::Other,
        ] {
            let io_err = io::Error::new(kind.to_io_kind(), "x");
            assert_eq!(
                WireError::from_io(&io_err).kind,
                kind,
                "{kind:?} is not its own round trip"
            );
        }
    }

    /// A non-zero `git` exit is a successful reply carrying that status, and it
    /// has to stay that way across the wire — this is the split every git call
    /// site downstream is built on.
    #[test]
    fn a_failing_git_is_ok_across_the_wire() {
        let p = pair();
        let tmp = tempfile::TempDir::new().unwrap();
        let Ok(out) = p.host.git(tmp.path(), &["rev-parse", "--show-toplevel"]) else {
            return; // no git on this machine
        };
        if out.success() {
            return; // the temp dir happens to live in a repository
        }
        assert!(out.status.is_some());
        assert!(
            !out.stderr.is_empty(),
            "stderr crosses the wire, not dropped"
        );
    }

    /// A frame the server cannot decode ends the connection rather than being
    /// skipped: a length-prefixed stream has no resynchronization point, so
    /// continuing would misread everything after it.
    #[test]
    fn an_unknown_frame_kind_ends_the_connection() {
        let (mut client, _) = raw();
        // Kind 99 is in neither space.
        crate::daemon::protocol::write_frame(&mut client, 99, b"junk").unwrap();
        client.flush().unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(
            client.read(&mut buf).unwrap(),
            0,
            "the server should hang up"
        );
    }

    /// A cancelled request produces no reply, and the connection carries on — a
    /// timed-out request must not leave a late frame that desyncs the peer's
    /// idea of what it is reading.
    #[test]
    fn a_cancelled_request_is_answered_with_silence() {
        let (mut client, _) = raw();
        let tmp = tempfile::TempDir::new().unwrap();

        // Cancel before the request, so the worker sees the flag on pickup.
        ControlClientMsg::Cancel { req_id: 1 }
            .encode(&mut client)
            .unwrap();
        ControlClientMsg::Request {
            req_id: 2,
            req: ControlRequest::Stat {
                path: tmp.path().to_string_lossy().into_owned(),
            },
        }
        .encode(&mut client)
        .unwrap();
        client.flush().unwrap();

        // Only request 2 is answered; a reply for the cancelled id would arrive
        // first and fail this.
        match ControlServerMsg::read(&mut client).unwrap() {
            ControlServerMsg::Response { req_id, reply } => {
                assert_eq!(req_id, 2);
                assert!(matches!(reply, ControlReply::Ok(ReplyOk::Meta(_))));
            }
            other => panic!("{other:?}"),
        }
    }

    /// The workspace store's request slots exist on the wire (so M5 is additive)
    /// but this server does not serve them, and says so instead of answering
    /// with something that looks like an empty store.
    #[test]
    fn the_workspace_store_is_declined_not_faked() {
        let p = pair();
        let err = p
            .host
            .client()
            .call(ControlRequest::WorkspaceList)
            .unwrap_err();
        assert!(err.to_string().contains("workspace store"), "{err}");
    }

    // -----------------------------------------------------------------------
    // Watches
    // -----------------------------------------------------------------------

    /// Watch batches reach the client as pushes, with the host's own coalescing
    /// window — the remote path has to behave exactly like the local one, which
    /// is why the server adds no window of its own.
    #[test]
    fn watch_batches_reach_the_client() {
        let p = pair();
        let tmp = tempfile::TempDir::new().unwrap();
        let sub = p.host.watch(&[tmp.path().to_path_buf()]).unwrap();
        while sub.events().try_recv().is_ok() {}

        let deadline = Instant::now() + Duration::from_secs(8);
        let mut seen = false;
        while Instant::now() < deadline && !seen {
            std::fs::write(tmp.path().join("hello.txt"), b"x").unwrap();
            std::thread::sleep(Duration::from_millis(200));
            while let Ok(batch) = sub.events().try_recv() {
                if batch
                    .iter()
                    .any(|q| q.file_name().is_some_and(|n| n == "hello.txt"))
                {
                    seen = true;
                }
            }
        }
        assert!(seen, "no watch event crossed the connection");
    }

    /// The `WatchId` reply is on the wire before any batch for that id.
    ///
    /// The ordering is structural — the forwarder is parked in
    /// `Conn::deferred_watches` and started by `finish`, so there is no
    /// interleaving left to hit. This is the end-to-end statement of that, run
    /// under churn; it does **not** reproduce the old race, whose window was a
    /// few instructions between `open_watch` returning and `finish` writing on
    /// the same thread.
    ///
    /// What the race cost, when it landed: the client files a watch id only
    /// once its `call` returns, so a batch that overtook the reply hit a client
    /// with no entry for that id and was dropped under the unknown-id rule. The
    /// file tree relists only on a watch event, so that first change stayed
    /// invisible until something else touched the directory.
    #[test]
    fn a_watch_id_reaches_the_client_before_any_batch_for_it() {
        let (mut client, _ok) = raw();
        let tmp = tempfile::TempDir::new().unwrap();

        ControlClientMsg::Request {
            req_id: 1,
            req: ControlRequest::WatchOpen {
                dirs: vec![tmp.path().to_string_lossy().into_owned()],
            },
        }
        .encode(&mut client)
        .unwrap();
        client.flush().unwrap();

        // Churn from the moment the request is sent, so the window between the
        // watcher going live and the reply going out is a busy one.
        let churn = tmp.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let churning = Arc::clone(&stop);
        let churner = std::thread::spawn(move || {
            let mut n = 0u32;
            while !churning.load(Ordering::SeqCst) {
                let _ = std::fs::write(churn.join(format!("f{n}")), b"x");
                n = n.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        // The very first frame back has to be the reply, not an event.
        let first = ControlServerMsg::read(&mut client).unwrap();
        stop.store(true, Ordering::SeqCst);
        churner.join().unwrap();

        match first {
            ControlServerMsg::Response {
                req_id: 1,
                reply: ControlReply::Ok(ReplyOk::WatchId(_)),
            } => {}
            other => panic!("a batch overtook the WatchId reply: {other:?}"),
        }
    }

    /// Dropping the subscription releases the *server's* watcher, not just the
    /// client's bookkeeping — otherwise every expanded directory in a long
    /// session leaks an OS watch on the remote machine.
    #[test]
    fn closing_a_watch_releases_it_on_the_server() {
        let (server, client) = UnixStream::pair().unwrap();
        let host = LocalHost::new();
        std::thread::spawn(move || {
            let _ = serve(server, host);
        });
        let hello = ControlHello::host_rpc("t", "h");
        let remote = RemoteHost::over_unix(client, "test:watch", &hello).unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let sub = remote.watch(&[tmp.path().to_path_buf()]).unwrap();
        let rx = sub.events().clone();
        drop(sub);

        // Give the close request time to reach the server, then prove the
        // watcher is gone by making a change nobody should hear about.
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(tmp.path().join("after.txt"), b"x").unwrap();
        std::thread::sleep(Duration::from_millis(800));
        while let Ok(batch) = rx.try_recv() {
            assert!(
                !batch
                    .iter()
                    .any(|q| q.file_name().is_some_and(|n| n == "after.txt")),
                "the server kept watching after the subscription closed"
            );
        }
    }

    /// A `.gitignore` edit invalidates the server's compiled matchers, and the
    /// client sees the new answer on its next listing — the invalidation lives in
    /// the host's watcher, so a remote client inherits it without asking.
    #[test]
    fn a_gitignore_edit_reaches_a_remote_listing() {
        let p = pair();
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        p.host
            .write_file(&root.join(".gitignore"), b"*.log\n")
            .unwrap();
        p.host.write_file(&root.join("a.log"), b"").unwrap();

        let listed = p.host.read_dir(&root, Some(&root)).unwrap();
        assert!(
            listed.iter().any(|e| e.name == "a.log" && e.ignored),
            "the fixture should start out ignored"
        );

        // The watch is what carries the invalidation, so it has to exist.
        let sub = p.host.watch(std::slice::from_ref(&root)).unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut cleared = false;
        while Instant::now() < deadline {
            p.host
                .write_file(&root.join(".gitignore"), b"# nothing\n")
                .unwrap();
            std::thread::sleep(Duration::from_millis(250));
            while sub.events().try_recv().is_ok() {}
            if p.host
                .read_dir(&root, Some(&root))
                .unwrap()
                .iter()
                .any(|e| e.name == "a.log" && !e.ignored)
            {
                cleared = true;
                break;
            }
        }
        assert!(
            cleared,
            "a `.gitignore` change on the server must reach the remote client's listing"
        );
    }

    /// A metadata shape the client asked for comes back as that shape, not a
    /// near miss — `write_file`'s reply carries the post-write mtime so the
    /// editor never needs a follow-up `stat`.
    #[test]
    fn a_write_answers_with_the_new_metadata() {
        let (mut client, _) = raw();
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("w.txt");

        ControlClientMsg::RequestBlob {
            req_id: 1,
            req: ControlRequest::WriteFile {
                path: f.to_string_lossy().into_owned(),
            },
            blob: b"hello".to_vec(),
        }
        .encode(&mut client)
        .unwrap();
        client.flush().unwrap();

        match ControlServerMsg::read(&mut client).unwrap() {
            ControlServerMsg::Response {
                reply: ControlReply::Ok(ReplyOk::Meta(m)),
                ..
            } => {
                assert_eq!(m.len, 5);
                assert!(
                    m.mtime.is_some(),
                    "the editor keys its echo detection on this"
                );
                let on_disk = std::fs::metadata(&f).unwrap();
                assert_eq!(
                    MTime::from_system_time(on_disk.modified().unwrap()),
                    m.mtime.unwrap()
                );
            }
            other => panic!("{other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // The socket
    // -----------------------------------------------------------------------

    /// The explicit override wins, which is what lets a test — or a second
    /// server on a shared box — have its own endpoint.
    #[test]
    fn the_socket_path_honours_its_override() {
        // Not `set_var` on the real environment: this binary runs its tests in
        // parallel threads and one of them would see the other's path.
        let dir = tempfile::TempDir::new().unwrap();
        let sock = dir.path().join("explicit.sock");
        let listener = bind_control_socket(&sock).unwrap();
        assert!(sock.exists());
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777,
            0o600,
            "a control socket is the access boundary; it must not be group- or world-reachable"
        );
        drop(listener);
    }

    /// A directory that was already there is left alone. `$TTY7_CONTROL_SOCK`
    /// and the hashed fallback both put the socket straight into
    /// `$XDG_RUNTIME_DIR` or `/tmp`; tightening one of those to 0700 would lock
    /// every other user out of it — sticky bit and all, if this is running as
    /// root — with nothing linking the breakage back to tty7.
    #[test]
    fn binding_does_not_re_permission_a_directory_it_did_not_create() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();

        let listener = bind_control_socket(&shared.join("s.sock")).unwrap();
        assert_eq!(
            std::fs::metadata(&shared).unwrap().permissions().mode() & 0o7777,
            0o1777,
            "a directory tty7 did not create must keep its own permissions"
        );
        drop(listener);
    }

    /// Owner-only comes from the umask the bind runs under, not from a `chmod`
    /// after the fact. A socket that spends even one syscall at `0777 & ~umask`
    /// is one another user on the box can connect to, and what is behind it is
    /// `ReadFile` on arbitrary paths as this user.
    #[test]
    fn a_bind_is_owner_only_under_a_permissive_umask() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("permissive.sock");

        // SAFETY: `umask` has no preconditions. It is process-global, so an
        // unrelated test creating a file in this window sees `0` rather than
        // the developer's umask — more permissive, which nothing asserts on.
        let previous = unsafe { libc::umask(0) };
        let bound = sock::bind_private(&path);
        unsafe { libc::umask(previous) };

        let listener = bound.unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
            0,
            "bind left a window in which another user could connect"
        );
        drop(listener);
    }

    /// One filename this wire cannot carry must not cost the whole search.
    ///
    /// Not driven through a real file: APFS rejects a non-UTF-8 name outright
    /// (`EILSEQ`), so the case cannot be staged on the machine most of this is
    /// developed on. The filter is the whole behaviour, so the filter is what
    /// is pinned — `to_frame_refuses_what_cannot_be_sent_without_writing_it`
    /// covers the other half, that such a hit really would fail to encode.
    #[test]
    fn a_filename_that_is_not_utf8_costs_only_its_own_hit() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let hit = |name: &str, path: &Path| SearchHit {
            name: name.to_string(),
            path: path.to_path_buf(),
            is_dir: false,
            ignored: false,
        };
        let mut hits = vec![
            hit("plain-needle.rs", Path::new("/home/me/plain-needle.rs")),
            // `café-needle.rs` in Latin-1: a lone 0xe9 is not valid UTF-8.
            hit(
                "caf\u{fffd}-needle.rs",
                Path::new(OsStr::from_bytes(b"/home/me/caf\xe9-needle.rs")),
            ),
            hit("other-needle.rs", Path::new("/home/me/other-needle.rs")),
        ];

        drop_unsendable_hits(&mut hits);
        let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(
            names,
            ["plain-needle.rs", "other-needle.rs"],
            "the representable hits must survive their neighbour"
        );
    }

    /// A whole conformance-shaped exchange over a real listener, proving the
    /// socket path end to end: bind, connect, handshake, RPC.
    #[test]
    fn a_socket_connection_serves_requests() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock = dir.path().join("s.sock");
        let listener = bind_control_socket(&sock).unwrap();
        std::thread::spawn(move || serve_listener(listener, LocalHost::new()));

        let stream = UnixStream::connect(&sock).unwrap();
        let hello = ControlHello::host_rpc("t", "h");
        let host = RemoteHost::over_unix(stream, "test:sock", &hello).unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        host.write_file(&tmp.path().join("x.txt"), b"content")
            .unwrap();
        assert_eq!(
            host.read_file(&tmp.path().join("x.txt"), 1024).unwrap(),
            b"content"
        );
        let entries = host.read_dir(tmp.path(), None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "x.txt");
    }

    /// Binding on top of a live server is refused rather than silently stealing
    /// its endpoint — two servers on one socket would each get an arbitrary half
    /// of the connections.
    #[test]
    fn binding_refuses_a_live_server() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock = dir.path().join("s.sock");
        let _first = bind_control_socket(&sock).unwrap();
        assert_eq!(
            bind_control_socket(&sock).unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
    }

    /// The natural path is used when it fits, and a too-long one falls back to a
    /// hashed short name — but only in a base that *itself* fits.
    ///
    /// The last part is the one worth a test: a deep `$XDG_RUNTIME_DIR` (a
    /// sandboxed CI runner, a nested container) makes the "short" fallback no
    /// shorter than what it replaced, and returning it anyway produces a `bind`
    /// failure that names `SUN_LEN` and nothing a reader could act on.
    #[test]
    fn a_too_long_socket_path_falls_back_to_one_that_fits() {
        let short = PathBuf::from("/tmp/rt");
        let long = PathBuf::from(format!("/tmp/{}", "d".repeat(120)));

        // Fits: used as-is, so an ordinary machine gets the readable path.
        assert_eq!(
            sock::socket_path_in(&short, &[]).unwrap(),
            PathBuf::from("/tmp/rt/daemon.sock")
        );

        // Too long: hashed into the first base that fits, skipping the one that
        // does not.
        let picked = sock::socket_path_in(&long, &[long.clone(), short.clone()]).unwrap();
        assert!(picked.starts_with(&short), "{}", picked.display());
        assert!(picked.to_string_lossy().ends_with(".sock"));

        // Distinct directories stay on distinct endpoints, or two servers would
        // collide on one socket.
        let other = PathBuf::from(format!("/tmp/{}", "e".repeat(120)));
        assert_ne!(
            picked,
            sock::socket_path_in(&other, std::slice::from_ref(&short)).unwrap()
        );

        // And it is deterministic: the bridge and the server derive it
        // independently, in different processes and possibly different builds.
        assert_eq!(
            picked,
            sock::socket_path_in(&long, &[long.clone(), short.clone()]).unwrap()
        );

        // Nowhere short enough is an error that says so, not a path that fails
        // later inside `bind`.
        let err = sock::socket_path_in(&long, std::slice::from_ref(&long)).unwrap_err();
        assert!(err.to_string().contains(CONTROL_SOCK_ENV), "{err}");
    }

    /// A socket file a crash left behind is cleared and rebound, because
    /// otherwise a server could never start again after one bad exit.
    #[test]
    fn binding_clears_a_socket_a_crash_left_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock = dir.path().join("s.sock");
        let first = bind_control_socket(&sock).unwrap();
        // Drop the listener but leave the file: exactly what a crash leaves.
        drop(first);
        assert!(sock.exists());

        // Darwin keeps answering `connect` on a closed listener for a short
        // while, and the probe genuinely cannot tell that from a live server —
        // so retry rather than assert on the first attempt. The production
        // caller is a process start-up, where a few milliseconds do not matter,
        // and the property under test is that the stale file is *eventually*
        // cleared rather than being fatal forever.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match bind_control_socket(&sock) {
                Ok(listener) => {
                    drop(listener);
                    return;
                }
                Err(e) if Instant::now() >= deadline => {
                    panic!("a stale control socket was never cleared: {e}")
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    // -----------------------------------------------------------------------
    // The workspace store
    // -----------------------------------------------------------------------

    /// A store on a temp file, plus the directory keeping it alive.
    fn temp_store() -> (Arc<WorkspaceStore>, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = WorkspaceStore::open(dir.path().join("workspaces.json"));
        (store, dir)
    }

    fn ws_record(id: &str, name: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "session": {"active": 0, "tabs": [
                {"pane": {"Leaf": {"cwd": "/home/me/proj", "pane_id": 3}}}
            ]},
            "last_active": 1_753_600_000u64,
        })
    }

    /// Issue one request and return its reply, ignoring any pushes that arrive
    /// first — a `WorkspaceChanged` from another connection can legitimately
    /// interleave with this one's reply.
    fn ask(client: &mut UnixStream, req_id: u64, req: ControlRequest) -> ControlReply {
        ControlClientMsg::Request { req_id, req }
            .encode(&mut *client)
            .unwrap();
        client.flush().unwrap();
        loop {
            match ControlServerMsg::read(&mut *client).unwrap() {
                ControlServerMsg::Response { req_id: got, reply } if got == req_id => {
                    return reply;
                }
                ControlServerMsg::Event(_) => continue,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }

    fn ok_json(reply: ControlReply) -> serde_json::Value {
        match reply {
            ControlReply::Ok(ReplyOk::Json(v)) => v,
            other => panic!("expected a Json reply, got {other:?}"),
        }
    }

    /// A server with no store answers the four slots the way it always has, and
    /// says so in the handshake so a client need not ask to find out.
    #[test]
    fn a_server_without_a_store_advertises_nothing_and_refuses_politely() {
        let (mut client, peer) = raw();
        assert!(!peer.has_feature(feature::WORKSPACE_STORE));
        assert!(peer.has_feature(feature::HOST_RPC));

        match ask(&mut client, 1, ControlRequest::WorkspaceList) {
            ControlReply::Err(e) => {
                assert_eq!(e.kind, WireErrorKind::Other);
                assert!(
                    e.msg.contains("does not serve the workspace store"),
                    "{e:?}"
                );
            }
            other => panic!("expected an error, got {other:?}"),
        }
        // And it is still a perfectly good file server afterwards: an
        // unsupported request must not poison the connection.
        assert!(matches!(
            ask(&mut client, 2, ControlRequest::Ping),
            ControlReply::Ok(ReplyOk::Pong)
        ));
    }

    /// The four RPCs, end to end over the wire, against a real file.
    #[test]
    fn the_four_workspace_rpcs_round_trip_over_the_wire() {
        let (store, dir) = temp_store();
        let (mut client, peer) = raw_with(Services::with_workspaces(Arc::clone(&store)));
        assert!(peer.has_feature(feature::WORKSPACE_STORE));

        // Empty to begin with.
        assert_eq!(
            ok_json(ask(&mut client, 1, ControlRequest::WorkspaceList)),
            serde_json::json!([])
        );

        // Put two.
        for (i, (id, name)) in [("w-a", "api"), ("w-b", "web")].iter().enumerate() {
            assert!(matches!(
                ask(
                    &mut client,
                    10 + i as u64,
                    ControlRequest::WorkspacePut {
                        id: (*id).to_string(),
                        json: ws_record(id, name),
                    },
                ),
                ControlReply::Ok(ReplyOk::Unit)
            ));
        }

        // Get one back, exactly as it was written.
        let got = ok_json(ask(
            &mut client,
            20,
            ControlRequest::WorkspaceGet {
                id: "w-a".to_string(),
            },
        ));
        assert_eq!(got, ws_record("w-a", "api"));

        // List answers an array in file order.
        let listed = ok_json(ask(&mut client, 21, ControlRequest::WorkspaceList));
        let ids: Vec<&str> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["w-a", "w-b"]);

        // A missing id is `NotFound`, not an empty payload.
        match ask(
            &mut client,
            22,
            ControlRequest::WorkspaceGet {
                id: "nope".to_string(),
            },
        ) {
            ControlReply::Err(e) => assert_eq!(e.kind, WireErrorKind::NotFound),
            other => panic!("expected NotFound, got {other:?}"),
        }

        // A record whose id disagrees with its key is refused.
        match ask(
            &mut client,
            23,
            ControlRequest::WorkspacePut {
                id: "w-a".to_string(),
                json: ws_record("w-b", "confused"),
            },
        ) {
            ControlReply::Err(e) => assert_eq!(e.kind, WireErrorKind::InvalidInput),
            other => panic!("expected InvalidInput, got {other:?}"),
        }

        // Delete, twice — the second is still success.
        for req_id in [30, 31] {
            assert!(matches!(
                ask(
                    &mut client,
                    req_id,
                    ControlRequest::WorkspaceDelete {
                        id: "w-a".to_string(),
                    },
                ),
                ControlReply::Ok(ReplyOk::Unit)
            ));
        }

        // The file on the server's disk is the authority, and it agrees.
        let text = std::fs::read_to_string(dir.path().join("workspaces.json")).unwrap();
        assert!(text.contains("w-b"), "{text}");
        assert!(!text.contains("w-a"), "{text}");
        assert_eq!(store.len(), 1);
    }

    /// **What the event exists for.** Two clients on one machine: a change made
    /// by one has to reach the other, and must not come back to its author as
    /// news it already has.
    #[test]
    fn a_change_reaches_the_other_client_and_not_its_author() {
        let (store, _dir) = temp_store();
        let services = Services::with_workspaces(Arc::clone(&store));
        let (mut writer, _) = raw_with(services.clone());
        let (mut listener, _) = raw_with(services);

        assert!(matches!(
            ask(
                &mut writer,
                1,
                ControlRequest::WorkspacePut {
                    id: "w".to_string(),
                    json: ws_record("w", "api"),
                },
            ),
            ControlReply::Ok(ReplyOk::Unit)
        ));

        // The listener is told which workspace to refetch.
        listener
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        match ControlServerMsg::read(&mut listener).unwrap() {
            ControlServerMsg::Event(ControlEvent::WorkspaceChanged { id }) => {
                assert_eq!(id, "w");
            }
            other => panic!("expected a WorkspaceChanged push, got {other:?}"),
        }

        // A delete is a change too.
        assert!(matches!(
            ask(
                &mut writer,
                2,
                ControlRequest::WorkspaceDelete {
                    id: "w".to_string(),
                },
            ),
            ControlReply::Ok(ReplyOk::Unit)
        ));
        match ControlServerMsg::read(&mut listener).unwrap() {
            ControlServerMsg::Event(ControlEvent::WorkspaceChanged { id }) => assert_eq!(id, "w"),
            other => panic!("expected a WorkspaceChanged push, got {other:?}"),
        }

        // The author heard nothing about either of its own writes: its next
        // frame is the reply to a fresh request, not a backlog of echoes.
        writer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        ControlClientMsg::Request {
            req_id: 3,
            req: ControlRequest::Ping,
        }
        .encode(&mut writer)
        .unwrap();
        writer.flush().unwrap();
        match ControlServerMsg::read(&mut writer).unwrap() {
            ControlServerMsg::Response { req_id: 3, reply } => {
                assert!(matches!(reply, ControlReply::Ok(ReplyOk::Pong)));
            }
            other => panic!("the author was pushed its own change: {other:?}"),
        }
    }

    /// A subscription is a connection's resource like any other: when the
    /// connection ends, the store must stop holding a callback into its sink.
    #[test]
    fn a_closed_connection_stops_being_a_subscriber() {
        let (store, _dir) = temp_store();
        let (server, client) = UnixStream::pair().unwrap();
        let served = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                let _ = serve_with(server, LocalHost::new(), Services::with_workspaces(store));
            })
        };
        // Handshake, then hang up.
        let mut client = client;
        ControlClientMsg::Hello(ControlHello::host_rpc("t", "h"))
            .encode(&mut client)
            .unwrap();
        client.flush().unwrap();
        let _ = ControlServerMsg::read(&mut client).unwrap();
        drop(client);
        served.join().unwrap();

        // The store still works, and writing to it does not try to reach a sink
        // that is gone. (A leaked subscriber would show up as a `BrokenPipe`
        // log rather than a failure, so the assertion is that the put succeeds
        // and the record lands.)
        store.put("w", ws_record("w", "api"), None).unwrap();
        assert_eq!(store.len(), 1);
    }

    /// A store shared by many connections writing at once: the server has to be
    /// as safe as the store is, and no request may be lost or answered twice.
    #[test]
    fn concurrent_connections_can_all_write_the_store() {
        let (store, _dir) = temp_store();
        let services = Services::with_workspaces(Arc::clone(&store));

        let writers: Vec<_> = (0..6)
            .map(|c| {
                let services = services.clone();
                std::thread::spawn(move || {
                    let (mut client, _) = raw_with(services);
                    for i in 0..10 {
                        let id = format!("c{c}-{i}");
                        let reply = ask(
                            &mut client,
                            i as u64 + 1,
                            ControlRequest::WorkspacePut {
                                id: id.clone(),
                                json: ws_record(&id, "x"),
                            },
                        );
                        assert!(
                            matches!(reply, ControlReply::Ok(ReplyOk::Unit)),
                            "{reply:?}"
                        );
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        assert_eq!(store.len(), 60);
    }

    // -----------------------------------------------------------------------
    // Attachment and takeover (D8)
    // -----------------------------------------------------------------------

    /// **D8 in one test.** The newcomer wins, the incumbent is *told* rather
    /// than silently dropped, and the workspace ends up held by exactly one
    /// session.
    ///
    /// The refusal design would pass an "only one client at a time" assertion
    /// just as well; what pins the decision is that the *second* connection is
    /// the one that ends up holding it.
    #[test]
    fn a_second_client_takes_the_workspace_and_the_first_is_told() {
        let (services, _dir) = workspace_services();
        let registry = Arc::clone(&services.attachments);

        let ((mut laptop, _), _laptop_served) =
            raw_hello(services.clone(), hello_for("w", "tok-laptop", "laptop"));
        await_holder(&registry, "w", "laptop");

        let ((mut desktop, _), _desktop_served) =
            raw_hello(services.clone(), hello_for("w", "tok-desktop", "desktop"));

        // The displaced session hears who took it, and which workspace: one
        // connection can carry several, so a push without the id would leave the
        // client unable to say which window went read-only.
        assert_eq!(
            await_preempted(&mut laptop),
            Some(ControlEvent::Preempted {
                workspace: "w".to_string(),
                by: "desktop".to_string(),
            })
        );
        assert_eq!(registry.holder("w").map(|(_, h)| h), Some("desktop".into()));
        assert_eq!(
            services
                .workspaces
                .as_ref()
                .unwrap()
                .attachment("w")
                .unwrap()
                .hostname,
            "desktop",
            "the store's record moves with the live handles"
        );

        // And the newcomer is told what it took over from — a takeover the new
        // client cannot see is one the user cannot explain.
        let (reply, _) = round_trip(
            &mut desktop,
            1,
            ControlRequest::WorkspaceAttach { id: "w".into() },
        );
        assert_eq!(
            reply,
            ControlReply::Ok(ReplyOk::Attached {
                took_over_from: None
            }),
            "re-attaching from the connection that already holds it is not a takeover"
        );
    }

    /// The displaced session's stream is closed, and when the
    /// connection exists for that one workspace that is exactly right.
    #[test]
    fn a_dedicated_connection_is_closed_when_its_workspace_is_taken() {
        let (services, _dir) = workspace_services();
        let ((mut laptop, _), _l) =
            raw_hello(services.clone(), hello_for("w", "tok-laptop", "laptop"));
        await_holder(&services.attachments, "w", "laptop");
        let ((_desktop, _), _d) =
            raw_hello(services.clone(), hello_for("w", "tok-desktop", "desktop"));

        assert!(await_preempted(&mut laptop).is_some());
        // The push comes first and the close after: the notice is useless if it
        // races the shutdown, which is why the order is fixed in the server.
        let mut sink = Vec::new();
        let _ = std::io::Read::read_to_end(&mut laptop, &mut sink);
        assert!(
            ControlServerMsg::read(&mut laptop).is_err(),
            "a dedicated connection is closed once its workspace is gone"
        );
    }

    /// Deleting a workspace clears it from *both* tables.
    ///
    /// The store drops its own attachment on delete. If the registry keeps its
    /// handle, the two disagree with no race needed, and the next client to
    /// attach that id evicts a session nobody displaced — closing its whole
    /// link, since a dedicated entry takes every other workspace on that
    /// connection down with it.
    #[test]
    fn deleting_a_workspace_clears_both_attachment_tables() {
        let (services, _dir) = workspace_services();
        let registry = Arc::clone(&services.attachments);
        let store = services.workspaces.clone().unwrap();

        let ((mut laptop, _), _l) =
            raw_hello(services.clone(), hello_for("w", "tok-laptop", "laptop"));
        await_holder(&registry, "w", "laptop");
        assert!(store.attachment("w").is_some());

        ask(
            &mut laptop,
            1,
            ControlRequest::WorkspacePut {
                id: "w".to_string(),
                json: ws_record("w", "the workspace"),
            },
        );
        let reply = ask(
            &mut laptop,
            2,
            ControlRequest::WorkspaceDelete {
                id: "w".to_string(),
            },
        );
        assert!(
            matches!(reply, ControlReply::Ok(ReplyOk::Unit)),
            "{reply:?}"
        );

        assert!(
            store.attachment("w").is_none(),
            "the store still names a holder for a workspace that is gone"
        );
        assert!(
            registry.holder("w").is_none(),
            "the registry still holds a workspace that is gone"
        );
    }

    /// The store's record and the registry's handle move under **one** lock.
    ///
    /// They are separate tables with separate locks, and taking them one after
    /// the other is not enough: two clients attaching the same workspace at the
    /// same instant can each win a different one, after which the store names a
    /// session the registry has already evicted. No `detach` can clear it — its
    /// token no longer matches — so from then on the workspace reports a
    /// takeover against a client that disconnected hours ago.
    ///
    /// Held from the test rather than raced, because the window is a few
    /// instructions wide and a racing test passes against the broken ordering
    /// far more often than it fails. Holding the handover proves the stronger
    /// thing anyway: with it held, an attach reaches *neither* table.
    #[test]
    fn an_attach_moves_both_tables_under_one_lock() {
        let (services, _dir) = workspace_services();
        let registry = Arc::clone(&services.attachments);
        let store = services.workspaces.clone().unwrap();

        let held = registry.handover();
        // The handshake replies before the attach, so this returns rather than
        // blocking on the lock we are holding.
        let ((_laptop, _ok), _served) =
            raw_hello(services.clone(), hello_for("w", "tok-laptop", "laptop"));

        std::thread::sleep(Duration::from_millis(150));
        assert!(
            registry.holder("w").is_none(),
            "the registry was moved while a handover was in flight"
        );
        assert!(
            store.attachment("w").is_none(),
            "the store was moved while a handover was in flight"
        );

        drop(held);
        await_holder(&registry, "w", "laptop");
        assert_eq!(
            store.attachment("w").map(|a| a.token).as_deref(),
            Some("tok-laptop"),
            "both tables have to name the same session once the handover is done"
        );
    }

    /// The other half of that rule. A client holds **one connection per
    /// machine**, so closing the link on a takeover would drop windows nobody
    /// preempted; the push still goes out, the link stays up.
    #[test]
    fn a_shared_connection_survives_losing_one_of_its_workspaces() {
        let (services, _dir) = workspace_services();
        let registry = Arc::clone(&services.attachments);

        let ((mut laptop, _), _l) = raw_hello(
            services.clone(),
            ControlHello::host_rpc("tok-laptop", "laptop"),
        );
        for (i, id) in ["w1", "w2"].iter().enumerate() {
            let (reply, _) = round_trip(
                &mut laptop,
                i as u64 + 1,
                ControlRequest::WorkspaceAttach { id: (*id).into() },
            );
            assert_eq!(
                reply,
                ControlReply::Ok(ReplyOk::Attached {
                    took_over_from: None
                })
            );
        }
        assert_eq!(registry.len(), 2);

        let ((_desktop, _), _d) =
            raw_hello(services.clone(), hello_for("w1", "tok-desktop", "desktop"));
        assert_eq!(
            await_preempted(&mut laptop),
            Some(ControlEvent::Preempted {
                workspace: "w1".to_string(),
                by: "desktop".to_string(),
            })
        );

        // Still alive, and still holding the workspace nobody touched.
        let (reply, _) = round_trip(&mut laptop, 9, ControlRequest::Ping);
        assert_eq!(reply, ControlReply::Ok(ReplyOk::Pong));
        assert_eq!(
            registry.holder("w2").map(|(t, _)| t),
            Some("tok-laptop".into())
        );
        assert_eq!(
            registry.holder("w1").map(|(t, _)| t),
            Some("tok-desktop".into())
        );
    }

    /// A preempted client tears down *after* the new one attached. An
    /// unconditional release would then evict the client that just took over,
    /// leaving the workspace looking free while a live window is on it.
    #[test]
    fn a_displaced_session_tidying_up_does_not_evict_the_new_owner() {
        let (services, _dir) = workspace_services();
        let registry = Arc::clone(&services.attachments);
        let store = Arc::clone(services.workspaces.as_ref().unwrap());

        let ((mut laptop, _), _l) = raw_hello(
            services.clone(),
            ControlHello::host_rpc("tok-laptop", "laptop"),
        );
        // Two workspaces on one link, which is what a client with two windows
        // on one machine has — and what keeps this link up once `w` is taken.
        for (i, id) in ["w", "other"].iter().enumerate() {
            round_trip(
                &mut laptop,
                i as u64 + 1,
                ControlRequest::WorkspaceAttach { id: (*id).into() },
            );
        }
        let ((_desktop, _), _d) =
            raw_hello(services.clone(), hello_for("w", "tok-desktop", "desktop"));
        assert!(await_preempted(&mut laptop).is_some());

        // The laptop, which no longer holds anything, tidies up.
        let (reply, _) = round_trip(
            &mut laptop,
            3,
            ControlRequest::WorkspaceDetach { id: "w".into() },
        );
        assert_eq!(reply, ControlReply::Ok(ReplyOk::Unit));
        assert_eq!(
            registry.holder("w").map(|(t, _)| t),
            Some("tok-desktop".into()),
            "the displaced session must not release what it no longer holds"
        );
        assert_eq!(store.attachment("w").unwrap().token, "tok-desktop");
    }

    /// A connection ending gives its workspaces back, so the next client does
    /// not see a takeover against a session that is gone.
    #[test]
    fn a_closed_connection_releases_what_it_held() {
        let (services, _dir) = workspace_services();
        let registry = Arc::clone(&services.attachments);
        let store = Arc::clone(services.workspaces.as_ref().unwrap());
        {
            let ((client, _), served) =
                raw_hello(services.clone(), hello_for("w", "tok", "laptop"));
            await_holder(&registry, "w", "laptop");
            drop(client);
            served.join().unwrap();
        }
        assert!(registry.is_empty(), "the registry outlived the connection");
        assert_eq!(store.attachment("w"), None);
    }

    /// A server with no workspace store has no workspaces to attach to, and
    /// says so rather than pretending the claim succeeded.
    #[test]
    fn attaching_to_a_server_without_a_store_is_an_error() {
        let (mut client, _) = raw_with(Services::none());
        let (reply, _) = round_trip(
            &mut client,
            1,
            ControlRequest::WorkspaceAttach { id: "w".into() },
        );
        assert!(matches!(reply, ControlReply::Err(_)), "{reply:?}");
    }
}

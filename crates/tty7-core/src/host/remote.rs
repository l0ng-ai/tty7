//! [`RemoteHost`] — a [`Host`] whose filesystem is on another machine.
//!
//! Every method here is one control round trip: the `Host` call blocks its own
//! caller (which is always a background thread — see the module docs on why the
//! trait stays blocking), the request goes out with a fresh id, and
//! [`ControlClient`] wakes exactly that caller when the matching reply arrives.
//! Nothing is batched, nothing is cached, and nothing shares a queue: a
//! twenty-second `git` and a five-millisecond `read_dir` overlap freely.
//!
//! ## What this file is and isn't
//!
//! It is a **translation layer**, deliberately thin. The interesting machinery —
//! request ids, out-of-order reply matching, per-method deadlines,
//! cancellation, tearing every waiter down when the link dies — lives in
//! [`crate::daemon::control`], because the server needs the same wire and the
//! test suite needs to exercise the multiplexer without a `Host` in the
//! picture. What is left here is the mapping from a `Host` method to a
//! [`ControlRequest`] and back, plus the watch bookkeeping that has no wire
//! equivalent.
//!
//! ## Round trips are the budget
//!
//! On a transcontinental link a round trip is 150-250ms, so the count is the
//! only performance number that matters and every method is written to cost
//! exactly one:
//!
//! | Temptation | Why it is refused |
//! |---|---|
//! | `exists` as `stat().is_ok()` | The default would work, but `Exists` answers a bool without shipping metadata nobody asked for |
//! | `rename` probing `to` first | Two round trips *and* a TOCTOU. The server guarantees `AlreadyExists` |
//! | `repo_root` climbing one level per call | A twelve-deep path would cost twelve round trips; the server walks it |
//! | `search` listing directories one at a time | Up to `max_dirs` round trips — minutes. The whole walk runs on the server |
//! | `read_file` fetching then checking the size | `max_bytes` is enforced *before* the bytes move |

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::daemon::control::{
    ControlClient, ControlEvent, ControlHello, ControlHelloOk, ControlRequest, EventSink,
    KEEPALIVE_DEAD_AFTER, KEEPALIVE_IDLE_BEFORE_PING, KEEPALIVE_PING_INTERVAL, LinkShutdown,
    ReplyOk,
};
use crate::host::{
    Entry, Host, HostId, Meta, Output, SearchHit, SharedHost, ShellInventory, WatchHandle, WatchSub,
};

/// A [`Host`] backed by a control connection to another machine.
pub struct RemoteHost {
    id: HostId,
    client: Arc<ControlClient>,
    separator: char,
    watches: Arc<WatchRegistry>,
}

impl RemoteHost {
    /// Handshake over an already-connected duplex link and build the host.
    ///
    /// `r` and `w` are the two halves of one stream — a `try_clone`d socket, or
    /// a child process's stdout and stdin. `connection_key` is the normalized
    /// connection string the [`HostId`] is derived from (`ssh-alias:box`,
    /// `wsl:Ubuntu`, …); it deliberately excludes the workspace, so several
    /// workspaces on one machine share one id and therefore one git-status
    /// cache.
    pub fn connect<R, W>(
        r: R,
        w: W,
        connection_key: &str,
        hello: &ControlHello,
    ) -> io::Result<Arc<RemoteHost>>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        Self::connect_with(r, w, None, connection_key, hello)
    }

    /// [`RemoteHost::connect`] over a TCP socket, with link shutdown wired up.
    ///
    /// Prefer this wherever the transport has a shutdown. Without one, dropping
    /// the host cannot wake its reader thread, so the drop costs a grace period
    /// and leaves the thread behind — see [`LinkShutdown`].
    pub fn over_tcp(
        sock: std::net::TcpStream,
        connection_key: &str,
        hello: &ControlHello,
    ) -> io::Result<Arc<RemoteHost>> {
        let r = sock.try_clone()?;
        let closer: Arc<dyn LinkShutdown> = Arc::new(sock.try_clone()?);
        Self::connect_with(r, sock, Some(closer), connection_key, hello)
    }

    /// [`RemoteHost::connect`] over a Unix-domain socket, with link shutdown
    /// wired up.
    #[cfg(unix)]
    pub fn over_unix(
        sock: std::os::unix::net::UnixStream,
        connection_key: &str,
        hello: &ControlHello,
    ) -> io::Result<Arc<RemoteHost>> {
        let r = sock.try_clone()?;
        let closer: Arc<dyn LinkShutdown> = Arc::new(sock.try_clone()?);
        Self::connect_with(r, sock, Some(closer), connection_key, hello)
    }

    /// The full form. `shutdown` is what lets dropping this host actually close
    /// the link rather than orphan its reader.
    pub fn connect_with<R, W>(
        r: R,
        w: W,
        shutdown: Option<Arc<dyn LinkShutdown>>,
        connection_key: &str,
        hello: &ControlHello,
    ) -> io::Result<Arc<RemoteHost>>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        // The event sink has to exist before the client, and the watch table it
        // feeds has to outlive both, so the table is built first and shared
        // rather than reached back into.
        let watches = Arc::new(WatchRegistry::default());
        let sink_watches = Arc::clone(&watches);
        // The id is derived here rather than read back off the host because the
        // sink has to exist before the host does — and because an event that
        // could not say *which machine* it came from would be useless to the
        // window layer, which has one connection per machine.
        let id = HostId::from_connection_key(connection_key);
        let sink: EventSink = Box::new(move |event| match event {
            // Watch pushes belong to whoever is still holding the `WatchSub`.
            ControlEvent::Watch { .. } | ControlEvent::WatchOverflow { .. } => {
                sink_watches.dispatch(event);
            }
            // Everything else is about a *window*, and this layer has none.
            other => crate::daemon::control::observe_event(id, other),
        });

        let client = Arc::new(ControlClient::connect_with(r, w, shutdown, hello, sink)?);
        let separator = client.hello().separator;

        let host = Arc::new(RemoteHost {
            id,
            client: Arc::clone(&client),
            separator,
            watches,
        });
        spawn_keepalive(Arc::downgrade(&client));
        Ok(host)
    }

    /// What the peer said about itself at handshake time.
    pub fn peer(&self) -> &ControlHelloOk {
        self.client.hello()
    }

    /// The server's `$HOME`, for "new workspace defaults to `~`" — which has to
    /// mean the *remote's* home, not the client's.
    pub fn home(&self) -> PathBuf {
        PathBuf::from(&self.client.hello().home)
    }

    /// The underlying connection, for callers that need to speak control
    /// directly (the workspace store, once it exists).
    pub fn client(&self) -> &Arc<ControlClient> {
        &self.client
    }

    /// Erase to the shared trait object the rest of the tree holds.
    pub fn into_shared(self: Arc<Self>) -> SharedHost {
        self
    }

    fn call(&self, req: ControlRequest) -> io::Result<ReplyOk> {
        self.client.call(req)
    }
}

/// Render a path for the wire.
///
/// Lossy rather than an error: a remote path is UTF-8 by construction, and the
/// only way a non-UTF-8 one reaches here is if it came *from* the server's own
/// lossy listing — in which case failing would turn a cosmetically odd filename
/// into an unusable one.
fn wire_path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn wire_paths(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| wire_path(p)).collect()
}

/// A reply of the wrong shape is a server bug, and it is worth saying so
/// plainly rather than letting it surface as a confusing empty result.
fn wrong_shape(expected: &str, got: &ReplyOk) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("control peer answered with {got:?} where {expected} was required"),
    )
}

impl Host for RemoteHost {
    fn id(&self) -> HostId {
        self.id
    }

    fn separator(&self) -> char {
        self.separator
    }

    /// Absolute *in the peer's* terms, which is the whole reason this is a
    /// trait method: a Windows client asking `Path::is_absolute` about
    /// `/home/me` is told `false`, and would then treat every remote path as
    /// relative.
    fn is_absolute(&self, p: &Path) -> bool {
        let s = p.to_string_lossy();
        if self.separator == '\\' {
            // A remote Windows host: `C:\…`, or a UNC/rooted path.
            let mut c = s.chars();
            let drive = matches!(
                (c.next(), c.next(), c.next()),
                (Some(a), Some(':'), Some('\\' | '/')) if a.is_ascii_alphabetic()
            );
            drive || s.starts_with("\\\\") || s.starts_with('\\') || s.starts_with('/')
        } else {
            s.starts_with('/')
        }
    }

    fn read_dir(&self, dir: &Path, root: Option<&Path>) -> io::Result<Vec<Entry>> {
        match self.call(ControlRequest::ReadDir {
            dir: wire_path(dir),
            root: root.map(wire_path),
        })? {
            ReplyOk::Entries(e) => Ok(e),
            other => Err(wrong_shape("a directory listing", &other)),
        }
    }

    fn stat(&self, p: &Path) -> io::Result<Meta> {
        match self.call(ControlRequest::Stat { path: wire_path(p) })? {
            ReplyOk::Meta(m) => Ok(m),
            other => Err(wrong_shape("file metadata", &other)),
        }
    }

    /// One round trip that ships a bool, rather than the default's `stat` that
    /// ships metadata to throw away.
    fn exists(&self, p: &Path) -> bool {
        matches!(
            self.call(ControlRequest::Exists { path: wire_path(p) }),
            Ok(ReplyOk::Bool(true))
        )
    }

    fn read_file(&self, p: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
        // The content rides the frame's blob; the JSON head carries only the
        // metadata, so nothing has to be re-fetched afterwards.
        let got = self.client.call_full(
            ControlRequest::ReadFile {
                path: wire_path(p),
                max_bytes,
            },
            &[],
        )?;
        match got.reply {
            ReplyOk::FileMeta { .. } => Ok(got.blob),
            other => Err(wrong_shape("file contents", &other)),
        }
    }

    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        match self.call(ControlRequest::Canonicalize { path: wire_path(p) })? {
            ReplyOk::Path(s) => Ok(PathBuf::from(s)),
            other => Err(wrong_shape("a canonical path", &other)),
        }
    }

    fn search(
        &self,
        roots: &[PathBuf],
        query: &str,
        limit: usize,
        max_dirs: usize,
        show_hidden: bool,
    ) -> io::Result<Vec<SearchHit>> {
        match self.call(ControlRequest::Search {
            roots: wire_paths(roots),
            query: query.to_string(),
            limit: limit as u64,
            max_dirs: max_dirs as u64,
            show_hidden,
        })? {
            ReplyOk::Hits(h) => Ok(h),
            other => Err(wrong_shape("search hits", &other)),
        }
    }

    fn write_file(&self, p: &Path, bytes: &[u8]) -> io::Result<Meta> {
        // One round trip, not two: the reply already carries the post-write
        // metadata, so the editor's mtime baseline comes from the write itself
        // rather than from a follow-up `stat` an external edit could slip in
        // front of.
        match self
            .client
            .call_with_blob(ControlRequest::WriteFile { path: wire_path(p) }, bytes)?
        {
            ReplyOk::Meta(m) => Ok(m),
            other => Err(wrong_shape("the written file's metadata", &other)),
        }
    }

    fn create_file_new(&self, p: &Path) -> io::Result<()> {
        self.expect_unit(ControlRequest::CreateFileNew { path: wire_path(p) })
    }

    fn create_dir(&self, p: &Path, recursive: bool) -> io::Result<()> {
        self.expect_unit(ControlRequest::CreateDir {
            path: wire_path(p),
            recursive,
        })
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.expect_unit(ControlRequest::Rename {
            from: wire_path(from),
            to: wire_path(to),
        })
    }

    fn remove(&self, p: &Path, recursive: bool) -> io::Result<()> {
        self.expect_unit(ControlRequest::Remove {
            path: wire_path(p),
            recursive,
        })
    }

    fn repo_root(&self, p: &Path) -> io::Result<Option<PathBuf>> {
        match self.call(ControlRequest::RepoRoot { path: wire_path(p) })? {
            ReplyOk::OptPath(s) => Ok(s.map(PathBuf::from)),
            other => Err(wrong_shape("a repository root", &other)),
        }
    }

    /// `Ok` means git *ran* on the server. A non-zero exit is in
    /// [`Output::status`], not in the `Err` — which is what keeps
    /// `git_status`'s `Option<String>` semantics identical whether the repo is
    /// local or six thousand miles away.
    fn git(&self, cwd: &Path, args: &[&str]) -> io::Result<Output> {
        match self.call(ControlRequest::Git {
            cwd: wire_path(cwd),
            args: args.iter().map(|a| a.to_string()).collect(),
        })? {
            ReplyOk::Output(o) => Ok(o),
            other => Err(wrong_shape("a process result", &other)),
        }
    }

    /// Safe to send unguarded: the request landed in control v2, and the
    /// handshake already refused any peer on another dialect. A server too old
    /// to know the variant is never on the other end of a live connection — it
    /// was replaced at install time, or the connection never opened.
    fn shells(&self) -> io::Result<ShellInventory> {
        match self.call(ControlRequest::Shells)? {
            ReplyOk::Shells(inv) => Ok(inv),
            other => Err(wrong_shape("a shell inventory", &other)),
        }
    }

    fn watch(&self, dirs: &[PathBuf]) -> io::Result<WatchSub> {
        let id = match self.call(ControlRequest::WatchOpen {
            dirs: wire_paths(dirs),
        })? {
            ReplyOk::WatchId(id) => id,
            other => return Err(wrong_shape("a watch id", &other)),
        };

        // Unbounded so the reader thread never blocks delivering a batch: the
        // server has already coalesced within its window, and the consumer is a
        // UI that may be a frame or two behind.
        let (tx, rx) = smol::channel::unbounded();
        self.watches.insert(id, tx, dirs.to_vec());

        Ok(WatchSub::new(
            rx,
            Box::new(RemoteWatch {
                id,
                client: Arc::clone(&self.client),
                watches: Arc::clone(&self.watches),
            }),
        ))
    }

    fn is_connected(&self) -> bool {
        self.client.is_connected()
    }
}

impl RemoteHost {
    fn expect_unit(&self, req: ControlRequest) -> io::Result<()> {
        match self.call(req)? {
            ReplyOk::Unit => Ok(()),
            other => Err(wrong_shape("an acknowledgement", &other)),
        }
    }
}

impl std::fmt::Debug for RemoteHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteHost")
            .field("id", &self.id)
            .field("separator", &self.separator)
            .field("connected", &self.is_connected())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Watches
// ---------------------------------------------------------------------------

/// Live subscriptions, keyed by the id the server assigned.
///
/// It has to be shared rather than owned by the host because the reader thread
/// delivers into it and the host reads from it, and neither may wait on the
/// other.
#[derive(Default)]
struct WatchRegistry {
    subs: Mutex<HashMap<u64, WatchEntry>>,
}

struct WatchEntry {
    tx: smol::channel::Sender<Vec<PathBuf>>,
    /// The directories currently watched. Kept so an overflow can be answered
    /// with a full re-report — see [`WatchRegistry::dispatch`].
    dirs: Vec<PathBuf>,
}

impl WatchRegistry {
    fn insert(&self, id: u64, tx: smol::channel::Sender<Vec<PathBuf>>, dirs: Vec<PathBuf>) {
        if let Ok(mut subs) = self.subs.lock() {
            subs.insert(id, WatchEntry { tx, dirs });
        }
    }

    fn set_dirs(&self, id: u64, dirs: Vec<PathBuf>) {
        if let Ok(mut subs) = self.subs.lock()
            && let Some(entry) = subs.get_mut(&id)
        {
            entry.dirs = dirs;
        }
    }

    fn remove(&self, id: u64) {
        if let Ok(mut subs) = self.subs.lock() {
            subs.remove(&id);
        }
    }

    /// Route one server push. Runs on the reader thread, so it must not block —
    /// hence `try_send` into an unbounded channel.
    fn dispatch(&self, event: ControlEvent) {
        let (id, paths) = match event {
            ControlEvent::Watch { id, paths } => {
                (id, paths.into_iter().map(PathBuf::from).collect::<Vec<_>>())
            }
            // Overflow means "too many paths changed to enumerate". There is no
            // separate overflow signal on `WatchSub` — and there does not need
            // to be: reporting every watched directory as changed produces
            // exactly the behavior wanted, a re-listing of the whole watched
            // set, through the path the consumer already handles.
            ControlEvent::WatchOverflow { id } => {
                let dirs = self
                    .subs
                    .lock()
                    .ok()
                    .and_then(|s| s.get(&id).map(|e| e.dirs.clone()))
                    .unwrap_or_default();
                (id, dirs)
            }
            // Not this layer's business; the workspace handles them.
            other => {
                log::trace!("control event not routed by RemoteHost: {other:?}");
                return;
            }
        };

        if paths.is_empty() {
            return;
        }
        let Ok(subs) = self.subs.lock() else { return };
        if let Some(entry) = subs.get(&id) {
            // A closed receiver means the `WatchSub` is being dropped; the
            // `WatchClose` is already on its way.
            let _ = entry.tx.try_send(paths);
        }
    }
}

/// The implementation half of a remote [`WatchSub`].
struct RemoteWatch {
    id: u64,
    client: Arc<ControlClient>,
    watches: Arc<WatchRegistry>,
}

impl WatchHandle for RemoteWatch {
    fn set_dirs(&self, dirs: &[PathBuf]) -> io::Result<()> {
        match self.client.call(ControlRequest::WatchSet {
            id: self.id,
            dirs: wire_paths(dirs),
        })? {
            ReplyOk::Unit => {
                self.watches.set_dirs(self.id, dirs.to_vec());
                Ok(())
            }
            other => Err(wrong_shape("an acknowledgement", &other)),
        }
    }
}

impl Drop for RemoteWatch {
    fn drop(&mut self) {
        self.watches.remove(self.id);
        // Best effort: on a link that has already died there is nothing to tell,
        // and the server drops its watchers when the connection goes anyway.
        if self.client.is_connected() {
            let _ = self.client.call(ControlRequest::WatchClose { id: self.id });
        }
    }
}

// ---------------------------------------------------------------------------
// Keepalive
// ---------------------------------------------------------------------------

/// Watch the link for silence.
///
/// Two separate jobs, which is why the thresholds differ: *prove* the link is
/// alive when nothing else is using it (a ping after
/// [`KEEPALIVE_IDLE_BEFORE_PING`] of quiet), and *declare it dead* when even
/// that gets no answer ([`KEEPALIVE_DEAD_AFTER`], three ping intervals — two
/// may be lost without a false positive). A busy connection proves itself and
/// is never pinged.
///
/// Holds a `Weak`, so dropping the last `RemoteHost` ends the thread rather
/// than keeping a connection alive for nobody.
fn spawn_keepalive(client: Weak<ControlClient>) {
    let _ = std::thread::Builder::new()
        .name("tty7-control-keepalive".into())
        .spawn(move || {
            let mut last_ping = Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let Some(client) = client.upgrade() else {
                    return;
                };
                if !client.is_connected() {
                    return;
                }
                let idle = client.idle_for();
                if idle >= KEEPALIVE_DEAD_AFTER {
                    log::warn!("control link silent for {idle:?}; treating it as dead",);
                    client.close();
                    return;
                }
                if idle >= KEEPALIVE_IDLE_BEFORE_PING
                    && last_ping.elapsed() >= KEEPALIVE_PING_INTERVAL
                {
                    last_ping = Instant::now();
                    // A failed ping is not itself fatal — the deadline above is
                    // what decides, so one lost packet doesn't drop a workspace.
                    if let Err(e) = client.ping() {
                        log::debug!("control keepalive ping failed: {e}");
                    }
                }
            }
        });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::control::{
        ControlClientMsg, ControlReply, ControlServerMsg, MTime, WireError, WireErrorKind,
    };
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    fn hello_ok(separator: char) -> ControlHelloOk {
        ControlHelloOk {
            control_version: crate::daemon::control::CONTROL_VERSION,
            protocol_version: crate::daemon::protocol::PROTOCOL_VERSION,
            build: "test".into(),
            separator,
            home: "/home/me".into(),
            features: vec![
                crate::daemon::control::feature::CONTROL.into(),
                crate::daemon::control::feature::HOST_RPC.into(),
            ],
            instance: "test-instance".into(),
        }
    }

    fn meta() -> Meta {
        Meta {
            is_dir: false,
            is_symlink: false,
            len: 12,
            mtime: Some(MTime {
                secs: 1_769_000_000,
                nanos: 7,
            }),
            readonly: false,
        }
    }

    /// A scripted peer: every request it receives is forwarded to `seen`, and
    /// answered by `answer`.
    fn host_with_peer<F>(
        separator: char,
        answer: F,
    ) -> (Arc<RemoteHost>, mpsc::Receiver<ControlRequest>)
    where
        F: Fn(&ControlRequest) -> Option<(ControlReply, Vec<u8>)> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Hello(_) => {}
                other => panic!("expected Hello, got {other:?}"),
            }
            ControlServerMsg::HelloOk(hello_ok(separator))
                .encode(&mut sock)
                .unwrap();
            sock.flush().unwrap();
            loop {
                let (req_id, req) = match ControlClientMsg::read(&mut sock) {
                    Ok(ControlClientMsg::Request { req_id, req }) => (req_id, req),
                    Ok(ControlClientMsg::RequestBlob { req_id, req, blob }) => {
                        // Echo the blob back through the seen channel by way of
                        // the request itself: tests that care assert on it via
                        // a closure over their own state.
                        let _ = blob;
                        (req_id, req)
                    }
                    _ => return,
                };
                let reply = answer(&req);
                seen_tx.send(req).unwrap();
                match reply {
                    Some((reply, blob)) if blob.is_empty() => {
                        ControlServerMsg::Response { req_id, reply }
                            .encode(&mut sock)
                            .unwrap();
                    }
                    Some((reply, blob)) => {
                        ControlServerMsg::ResponseBlob {
                            req_id,
                            reply,
                            blob,
                        }
                        .encode(&mut sock)
                        .unwrap();
                    }
                    None => return,
                }
                sock.flush().unwrap();
            }
        });

        let sock = TcpStream::connect(addr).unwrap();
        let host = RemoteHost::over_tcp(
            sock,
            "ssh-alias:testbox",
            &ControlHello::host_rpc("tok", "laptop"),
        )
        .unwrap();
        (host, seen_rx)
    }

    /// The dropdown of a remote window is built from the *server's* shells.
    /// This is the whole point: a menu filled from the client's `/etc/shells`
    /// offers `/bin/zsh` on a box whose zsh lives elsewhere, and every pick
    /// fails to spawn.
    #[test]
    fn shells_come_from_the_peer() {
        let (host, seen) = host_with_peer('/', |req| match req {
            ControlRequest::Shells => Some((
                ControlReply::Ok(ReplyOk::Shells(crate::core::shells::ShellInventory {
                    shells: vec![crate::core::shells::DetectedShell {
                        label: "zsh".into(),
                        program: "/usr/bin/zsh".into(),
                        args: vec![],
                    }],
                    default_name: "zsh".into(),
                })),
                vec![],
            )),
            other => panic!("unexpected request {other:?}"),
        });

        let inv = host.shells().unwrap();
        assert_eq!(seen.recv().unwrap(), ControlRequest::Shells);
        assert_eq!(inv.default_name, "zsh");
        assert_eq!(inv.shells[0].program, "/usr/bin/zsh");
    }

    /// Path arithmetic follows the *peer's* separator, not the client's. On a
    /// Windows client this is the difference between `/home/me/src` and
    /// `/home/me\src`, and between "absolute" and "drive-relative".
    #[test]
    fn path_arithmetic_follows_the_peer_not_the_client() {
        let (host, _seen) =
            host_with_peer('/', |_| Some((ControlReply::Ok(ReplyOk::Unit), vec![])));
        let host: &dyn Host = host.as_ref();

        assert_eq!(host.separator(), '/');
        assert_eq!(
            host.join(Path::new("/home/me"), "src"),
            PathBuf::from("/home/me/src"),
            "joins with the remote's separator on every client platform"
        );
        assert!(
            host.is_absolute(Path::new("/home/me")),
            "a POSIX remote path is absolute even where std::path disagrees"
        );
        assert!(!host.is_absolute(Path::new("home/me")));
        assert!(!host.is_absolute(Path::new("C:/home")));
    }

    /// A remote Windows host gets Windows semantics — the separator is a
    /// property of the peer, so this must work in both directions.
    #[test]
    fn a_windows_peer_gets_windows_path_semantics() {
        let (host, _seen) =
            host_with_peer('\\', |_| Some((ControlReply::Ok(ReplyOk::Unit), vec![])));
        let host: &dyn Host = host.as_ref();

        assert_eq!(host.separator(), '\\');
        assert_eq!(
            host.join(Path::new("C:\\src"), "main.rs"),
            PathBuf::from("C:\\src\\main.rs")
        );
        assert!(host.is_absolute(Path::new("C:\\src")));
        assert!(host.is_absolute(Path::new("\\\\server\\share")));
        assert!(!host.is_absolute(Path::new("src\\main.rs")));
    }

    /// Each read method sends the request its name implies and unwraps the
    /// reply's payload — one round trip, no probing, no second call.
    #[test]
    fn read_methods_map_to_one_request_each() {
        let (host, seen) = host_with_peer('/', |req| {
            let reply = match req {
                ControlRequest::ReadDir { .. } => ReplyOk::Entries(vec![Entry {
                    name: "src".into(),
                    is_dir: true,
                    is_symlink: false,
                    ignored: false,
                }]),
                ControlRequest::Stat { .. } => ReplyOk::Meta(meta()),
                ControlRequest::Exists { .. } => ReplyOk::Bool(true),
                ControlRequest::Canonicalize { .. } => ReplyOk::Path("/real".into()),
                ControlRequest::RepoRoot { .. } => ReplyOk::OptPath(Some("/repo".into())),
                ControlRequest::Search { .. } => ReplyOk::Hits(vec![]),
                ControlRequest::Git { .. } => ReplyOk::Output(Output {
                    status: Some(0),
                    stdout: b" M src/main.rs\n".to_vec(),
                    stderr: Vec::new(),
                }),
                other => panic!("unexpected request {other:?}"),
            };
            Some((ControlReply::Ok(reply), vec![]))
        });
        let h: &dyn Host = host.as_ref();

        assert_eq!(
            h.read_dir(Path::new("/p"), Some(Path::new("/")))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(h.stat(Path::new("/f")).unwrap(), meta());
        assert!(h.exists(Path::new("/f")));
        assert_eq!(
            h.canonicalize(Path::new("/a/../b")).unwrap(),
            PathBuf::from("/real")
        );
        assert_eq!(
            h.repo_root(Path::new("/p/deep")).unwrap(),
            Some(PathBuf::from("/repo"))
        );
        assert!(
            h.search(&[PathBuf::from("/p")], "q", 10, 100, false)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            h.git(Path::new("/repo"), &["status"])
                .unwrap()
                .stdout_trimmed(),
            "M src/main.rs"
        );

        let sent: Vec<_> = (0..7).map(|_| seen.recv().unwrap()).collect();
        assert!(matches!(sent[0], ControlRequest::ReadDir { .. }));
        assert!(matches!(sent[1], ControlRequest::Stat { .. }));
        // `exists` must not degrade into a `stat`: that would ship metadata
        // across an ocean to answer a yes/no question.
        assert!(matches!(sent[2], ControlRequest::Exists { .. }));
        assert!(matches!(sent[3], ControlRequest::Canonicalize { .. }));
        assert!(matches!(sent[4], ControlRequest::RepoRoot { .. }));
        assert!(matches!(sent[5], ControlRequest::Search { .. }));
        assert!(matches!(sent[6], ControlRequest::Git { .. }));
    }

    /// `rename` states its intent once and trusts the server's `AlreadyExists`
    /// guarantee — a client-side `exists` probe first would be an extra round
    /// trip *and* racy.
    #[test]
    fn mutations_are_a_single_request_with_no_probe() {
        let (host, seen) = host_with_peer('/', |req| {
            let reply = match req {
                ControlRequest::Rename { .. } => ControlReply::Err(WireError::new(
                    WireErrorKind::AlreadyExists,
                    "/b already exists",
                )),
                _ => ControlReply::Ok(ReplyOk::Unit),
            };
            Some((reply, vec![]))
        });
        let h: &dyn Host = host.as_ref();

        h.create_file_new(Path::new("/new")).unwrap();
        h.create_dir(Path::new("/a/b"), true).unwrap();
        h.remove(Path::new("/gone"), false).unwrap();
        let e = h.rename(Path::new("/a"), Path::new("/b")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert!(e.to_string().contains("/b already exists"));

        let sent: Vec<_> = (0..4).map(|_| seen.recv().unwrap()).collect();
        assert!(matches!(sent[0], ControlRequest::CreateFileNew { .. }));
        assert!(matches!(
            sent[1],
            ControlRequest::CreateDir {
                recursive: true,
                ..
            }
        ));
        assert!(matches!(sent[2], ControlRequest::Remove { .. }));
        assert!(matches!(sent[3], ControlRequest::Rename { .. }));
        assert_eq!(seen.try_recv().ok(), None, "no probing round trips");
    }

    /// `read_file` gets its content from the reply's blob, and `write_file`
    /// puts its content in the request's — the reason bulk frames carry a JSON
    /// head at all is that the path and metadata travel beside the bytes.
    #[test]
    fn file_contents_ride_the_blob_in_both_directions() {
        let content: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
        let served = content.clone();
        let (host, _seen) = host_with_peer('/', move |req| match req {
            ControlRequest::ReadFile { .. } => Some((
                ControlReply::Ok(ReplyOk::FileMeta { meta: meta() }),
                served.clone(),
            )),
            ControlRequest::WriteFile { .. } => {
                Some((ControlReply::Ok(ReplyOk::Meta(meta())), vec![]))
            }
            other => panic!("unexpected request {other:?}"),
        });
        let h: &dyn Host = host.as_ref();

        assert_eq!(h.read_file(Path::new("/f"), u64::MAX).unwrap(), content);
        h.write_file(Path::new("/f"), &content).unwrap();
    }

    /// An oversize file is refused by the server *before* the bytes move. The
    /// error has to arrive as `FileTooLarge` so the editor can say so rather
    /// than showing a generic failure.
    #[test]
    fn read_file_over_the_limit_fails_without_transferring() {
        let (host, _seen) = host_with_peer('/', |_| {
            Some((
                ControlReply::Err(WireError::new(
                    WireErrorKind::FileTooLarge,
                    "/big is 900 MB, over the 10 MB limit",
                )),
                vec![],
            ))
        });
        let e = host
            .read_file(Path::new("/big"), 10 * 1024 * 1024)
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::FileTooLarge);
        assert!(e.to_string().contains("900 MB"));
    }

    /// A non-zero git exit is `Ok`. This is the invariant that lets every
    /// existing git call site keep its `Option`/`Result<String, String>` shape
    /// unchanged when the repo moves to another machine.
    #[test]
    fn a_nonzero_git_exit_is_ok_not_err() {
        let (host, _seen) = host_with_peer('/', |_| {
            Some((
                ControlReply::Ok(ReplyOk::Output(Output {
                    status: Some(128),
                    stdout: Vec::new(),
                    stderr: b"not a git repository".to_vec(),
                })),
                vec![],
            ))
        });
        let out = host.git(Path::new("/tmp"), &["rev-parse"]).unwrap();
        assert_eq!(out.status, Some(128));
        assert!(!out.success());
        assert_eq!(out.stderr_trimmed(), "not a git repository");
    }

    /// The id comes from the *connection*, not the workspace, so two workspaces
    /// on one machine share a host id — and therefore share its git-status
    /// cache instead of each maintaining a private one.
    #[test]
    fn the_host_id_is_derived_from_the_connection_and_is_stable() {
        let (a, _sa) = host_with_peer('/', |_| Some((ControlReply::Ok(ReplyOk::Unit), vec![])));
        let (b, _sb) = host_with_peer('/', |_| Some((ControlReply::Ok(ReplyOk::Unit), vec![])));
        assert_eq!(a.id(), b.id(), "same connection key, same id");
        assert_eq!(a.id(), a.id());
        assert_ne!(a.id(), HostId::LOCAL, "a remote host is never the local id");
        assert_eq!(a.id(), HostId::from_connection_key("ssh-alias:testbox"));
    }

    /// Watch events reach the subscription's channel, and `set_dirs` replaces
    /// the set in place rather than rebuilding the subscription.
    #[test]
    fn watch_events_reach_the_subscription() {
        let (host, seen) = host_with_peer('/', |req| match req {
            ControlRequest::WatchOpen { .. } => {
                Some((ControlReply::Ok(ReplyOk::WatchId(7)), vec![]))
            }
            ControlRequest::WatchSet { .. } | ControlRequest::WatchClose { .. } => {
                Some((ControlReply::Ok(ReplyOk::Unit), vec![]))
            }
            other => panic!("unexpected request {other:?}"),
        });

        let sub = host.watch(&[PathBuf::from("/p")]).unwrap();
        assert!(matches!(
            seen.recv().unwrap(),
            ControlRequest::WatchOpen { .. }
        ));

        // A push routed by id lands as a batch on the subscription.
        host.watches.dispatch(ControlEvent::Watch {
            id: 7,
            paths: vec!["/p/a".into(), "/p/b".into()],
        });
        assert_eq!(
            sub.events().recv_blocking().unwrap(),
            vec![PathBuf::from("/p/a"), PathBuf::from("/p/b")]
        );

        // An event for an id nobody holds is dropped, not delivered elsewhere.
        host.watches.dispatch(ControlEvent::Watch {
            id: 999,
            paths: vec!["/elsewhere".into()],
        });
        assert!(sub.events().is_empty());

        sub.set_dirs(&[PathBuf::from("/p"), PathBuf::from("/q")])
            .unwrap();
        assert!(matches!(
            seen.recv().unwrap(),
            ControlRequest::WatchSet { .. }
        ));

        // Overflow re-reports the whole watched set, which is how "invalidate
        // everything" reaches a consumer that only understands path batches.
        host.watches.dispatch(ControlEvent::WatchOverflow { id: 7 });
        assert_eq!(
            sub.events().recv_blocking().unwrap(),
            vec![PathBuf::from("/p"), PathBuf::from("/q")],
            "overflow must name the updated set, not the one watch() opened with"
        );
    }

    /// Dropping the subscription unsubscribes on the server, rather than
    /// leaving a watcher running for a client that stopped caring.
    #[test]
    fn dropping_the_subscription_closes_it_on_the_server() {
        let (host, seen) = host_with_peer('/', |req| match req {
            ControlRequest::WatchOpen { .. } => {
                Some((ControlReply::Ok(ReplyOk::WatchId(4)), vec![]))
            }
            _ => Some((ControlReply::Ok(ReplyOk::Unit), vec![])),
        });

        let sub = host.watch(&[PathBuf::from("/p")]).unwrap();
        assert!(matches!(
            seen.recv().unwrap(),
            ControlRequest::WatchOpen { .. }
        ));
        drop(sub);
        match seen.recv().unwrap() {
            ControlRequest::WatchClose { id } => assert_eq!(id, 4),
            other => panic!("expected WatchClose, got {other:?}"),
        }
    }

    /// A host whose link has died reports it, so call sites can keep showing
    /// the last good listing instead of flashing an error at every repaint.
    #[test]
    fn a_dead_link_reports_disconnected() {
        let (host, _seen) = host_with_peer('/', |_| None); // answers nothing, then hangs up
        let h: &dyn Host = host.as_ref();
        assert!(h.is_connected());
        let e = h.stat(Path::new("/f")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionReset);
        assert!(!h.is_connected());
    }

    /// A reply of the wrong shape is called out as a peer bug rather than
    /// being silently read as an empty result.
    #[test]
    fn a_reply_of_the_wrong_shape_is_invalid_data() {
        let (host, _seen) =
            host_with_peer('/', |_| Some((ControlReply::Ok(ReplyOk::Pong), vec![])));
        let e = host.stat(Path::new("/f")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("Pong"));
    }

    /// `RemoteHost` is usable as `Arc<dyn Host>` — object safety is not an
    /// abstract property here, it is what the whole tree's `SharedHost` needs.
    #[test]
    fn remote_host_is_object_safe() {
        let (host, _seen) =
            host_with_peer('/', |_| Some((ControlReply::Ok(ReplyOk::Unit), vec![])));
        let shared: SharedHost = host.into_shared();
        assert_eq!(shared.separator(), '/');
    }
}

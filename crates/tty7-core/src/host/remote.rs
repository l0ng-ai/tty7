use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::time::{Duration, Instant};

use crate::daemon::control::{
    ControlClient, ControlEvent, ControlHello, ControlHelloOk, ControlRequest, EventSink,
    GIT_STREAM_IDLE_TIMEOUT, KEEPALIVE_DEAD_AFTER, KEEPALIVE_IDLE_BEFORE_PING,
    KEEPALIVE_PING_INTERVAL, LinkShutdown, ReplyOk,
};
use crate::host::{
    Entry, Host, HostId, Meta, Output, SearchHit, SharedHost, ShellInventory, WatchHandle, WatchSub,
};

pub struct RemoteHost {
    id: HostId,
    client: Arc<ControlClient>,
    separator: char,
    watches: Arc<WatchRegistry>,
    streams: Arc<GitStreamRegistry>,
    next_stream: AtomicU64,
}

impl RemoteHost {
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

    pub fn over_tcp(
        sock: std::net::TcpStream,
        connection_key: &str,
        hello: &ControlHello,
    ) -> io::Result<Arc<RemoteHost>> {
        let r = sock.try_clone()?;
        let closer: Arc<dyn LinkShutdown> = Arc::new(sock.try_clone()?);
        Self::connect_with(r, sock, Some(closer), connection_key, hello)
    }

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
        let watches = Arc::new(WatchRegistry::default());
        let sink_watches = Arc::clone(&watches);
        let streams: Arc<GitStreamRegistry> = Arc::new(GitStreamRegistry::default());
        let sink_streams = Arc::clone(&streams);
        let id = HostId::from_connection_key(connection_key);
        let sink: EventSink = Box::new(move |event| match event {
            ControlEvent::Watch { .. } | ControlEvent::WatchOverflow { .. } => {
                sink_watches.dispatch(event);
            }
            ControlEvent::GitChunk { .. } | ControlEvent::GitEnd { .. } => {
                sink_streams.dispatch(event);
            }
            other => crate::daemon::control::observe_event(id, other),
        });

        let client = Arc::new(ControlClient::connect_with(r, w, shutdown, hello, sink)?);
        let separator = client.hello().separator;
        let down_streams = Arc::clone(&streams);
        client.on_link_down(move || down_streams.close_all());

        let host = Arc::new(RemoteHost {
            id,
            streams,
            next_stream: AtomicU64::new(1),
            client: Arc::clone(&client),
            separator,
            watches,
        });
        spawn_keepalive(Arc::downgrade(&client));
        Ok(host)
    }

    pub fn peer(&self) -> &ControlHelloOk {
        self.client.hello()
    }

    pub fn home(&self) -> PathBuf {
        PathBuf::from(&self.client.hello().home)
    }

    pub fn client(&self) -> &Arc<ControlClient> {
        &self.client
    }

    pub fn into_shared(self: Arc<Self>) -> SharedHost {
        self
    }

    fn call(&self, req: ControlRequest) -> io::Result<ReplyOk> {
        self.client.call(req)
    }
}

fn wire_path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn wire_paths(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| wire_path(p)).collect()
}

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

    fn is_absolute(&self, p: &Path) -> bool {
        let s = p.to_string_lossy();
        if self.separator == '\\' {
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

    fn exists(&self, p: &Path) -> bool {
        matches!(
            self.call(ControlRequest::Exists { path: wire_path(p) }),
            Ok(ReplyOk::Bool(true))
        )
    }

    fn read_file(&self, p: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
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

    fn git(&self, cwd: &Path, args: &[&str]) -> io::Result<Output> {
        match self.call(ControlRequest::Git {
            cwd: wire_path(cwd),
            args: args.iter().map(|a| a.to_string()).collect(),
        })? {
            ReplyOk::Output(o) => Ok(o),
            other => Err(wrong_shape("a process result", &other)),
        }
    }

    fn git_lines(
        &self,
        cwd: &Path,
        args: &[&str],
        on_line: &mut dyn FnMut(&str),
    ) -> io::Result<Option<i32>> {
        let id = self.next_stream.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        self.streams.insert(
            id,
            StreamSink {
                tx,
                queued: Arc::clone(&queued),
            },
        );
        let _guard = StreamGuard {
            streams: &self.streams,
            id,
        };

        match self.call(ControlRequest::GitStream {
            id,
            cwd: wire_path(cwd),
            args: args.iter().map(|a| a.to_string()).collect(),
        })? {
            ReplyOk::Unit => {}
            other => return Err(wrong_shape("an accepted git stream", &other)),
        }

        drain_git_stream(
            &mut |idle| rx.recv_timeout(idle),
            &queued,
            GIT_STREAM_IDLE_TIMEOUT,
            on_line,
        )
    }

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

const GIT_STREAM_QUEUE_BUDGET: usize = 32 * 1024 * 1024;

type NextGitMsg<'a> = &'a mut dyn FnMut(Duration) -> Result<GitStreamMsg, mpsc::RecvTimeoutError>;

fn drain_git_stream(
    next: NextGitMsg<'_>,
    queued: &AtomicUsize,
    idle: Duration,
    on_line: &mut dyn FnMut(&str),
) -> io::Result<Option<i32>> {
    let mut split = crate::core::git::LineSplitter::default();
    loop {
        match next(idle) {
            Ok(GitStreamMsg::Chunk(bytes)) => {
                let _ = queued.fetch_update(Ordering::AcqRel, Ordering::Acquire, |q| {
                    Some(q.saturating_sub(bytes.len()))
                });
                split.push(&bytes, &mut *on_line);
            }
            Ok(GitStreamMsg::Overrun) => {
                return Err(io::Error::other(format!(
                    "the git stream outran this client by more than \
                     {GIT_STREAM_QUEUE_BUDGET} queued bytes"
                )));
            }
            Ok(GitStreamMsg::End { code, failed }) => {
                split.finish(&mut *on_line);
                return if failed {
                    Err(io::Error::other("the server could not run git"))
                } else {
                    Ok(code)
                };
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("the git stream went silent for {idle:?} while the link stayed up"),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("the control connection closed mid-stream"));
            }
        }
    }
}

struct StreamGuard<'a> {
    streams: &'a GitStreamRegistry,
    id: u64,
}

impl Drop for StreamGuard<'_> {
    fn drop(&mut self) {
        self.streams.remove(self.id);
    }
}

enum GitStreamMsg {
    Chunk(Vec<u8>),
    End { code: Option<i32>, failed: bool },
    Overrun,
}

struct StreamSink {
    tx: mpsc::Sender<GitStreamMsg>,
    queued: Arc<AtomicUsize>,
}

#[derive(Default)]
struct GitStreamRegistry {
    streams: Mutex<StreamTable>,
}

#[derive(Default)]
struct StreamTable {
    senders: HashMap<u64, StreamSink>,
    closed: bool,
}

impl GitStreamRegistry {
    fn insert(&self, id: u64, sink: StreamSink) {
        if let Ok(mut m) = self.streams.lock()
            && !m.closed
        {
            m.senders.insert(id, sink);
        }
    }

    fn remove(&self, id: u64) {
        if let Ok(mut m) = self.streams.lock() {
            m.senders.remove(&id);
        }
    }

    fn close_all(&self) {
        let Ok(mut m) = self.streams.lock() else {
            return;
        };
        m.closed = true;
        m.senders.clear();
    }

    fn dispatch(&self, event: ControlEvent) {
        let (id, msg) = match event {
            ControlEvent::GitChunk { id, bytes } => (id, GitStreamMsg::Chunk(bytes)),
            ControlEvent::GitEnd { id, code, failed } => (id, GitStreamMsg::End { code, failed }),
            _ => return,
        };
        let Ok(mut m) = self.streams.lock() else {
            return;
        };
        let over = match (m.senders.get(&id), &msg) {
            (None, _) => return,
            (Some(sink), GitStreamMsg::Chunk(bytes)) => {
                sink.queued.fetch_add(bytes.len(), Ordering::AcqRel) + bytes.len()
                    > GIT_STREAM_QUEUE_BUDGET
            }
            (Some(_), _) => false,
        };
        if over {
            if let Some(sink) = m.senders.remove(&id) {
                let _ = sink.tx.send(GitStreamMsg::Overrun);
            }
            return;
        }
        if let Some(sink) = m.senders.get(&id) {
            let _ = sink.tx.send(msg);
        }
    }
}

#[derive(Default)]
struct WatchRegistry {
    subs: Mutex<HashMap<u64, WatchEntry>>,
}

struct WatchEntry {
    tx: smol::channel::Sender<Vec<PathBuf>>,
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

    fn dispatch(&self, event: ControlEvent) {
        let (id, paths) = match event {
            ControlEvent::Watch { id, paths } => {
                (id, paths.into_iter().map(PathBuf::from).collect::<Vec<_>>())
            }
            ControlEvent::WatchOverflow { id } => {
                let dirs = self
                    .subs
                    .lock()
                    .ok()
                    .and_then(|s| s.get(&id).map(|e| e.dirs.clone()))
                    .unwrap_or_default();
                (id, dirs)
            }
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
            let _ = entry.tx.try_send(paths);
        }
    }
}

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
        if self.client.is_connected() {
            let _ = self.client.call(ControlRequest::WatchClose { id: self.id });
        }
    }
}

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
                    if let Err(e) = client.ping() {
                        log::debug!("control keepalive ping failed: {e}");
                    }
                }
            }
        });
}

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

    #[test]
    fn git_lines_streams_over_the_wire() {
        let (host, seen) = host_with_peer_streaming();

        let mut lines = Vec::new();
        let code = host
            .git_lines(Path::new("/repo"), &["diff"], &mut |l| {
                lines.push(l.to_string())
            })
            .unwrap();

        assert_eq!(code, Some(0));
        assert_eq!(lines, ["alpha", "beta", "gamma"]);
        match seen.recv().unwrap() {
            ControlRequest::GitStream { id, cwd, args } => {
                assert_eq!(id, 1, "the client picks the id, starting at 1");
                assert_eq!(cwd, "/repo");
                assert_eq!(args, ["diff"]);
            }
            other => panic!("expected a GitStream request, got {other:?}"),
        }
    }

    fn host_with_peer_streaming() -> (Arc<RemoteHost>, mpsc::Receiver<ControlRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Hello(_) => {}
                other => panic!("expected Hello, got {other:?}"),
            }
            ControlServerMsg::HelloOk(hello_ok('/'))
                .encode(&mut sock)
                .unwrap();
            sock.flush().unwrap();
            loop {
                let (req_id, req) = match ControlClientMsg::read(&mut sock) {
                    Ok(ControlClientMsg::Request { req_id, req }) => (req_id, req),
                    _ => return,
                };
                let id = match &req {
                    ControlRequest::GitStream { id, .. } => *id,
                    other => panic!("expected GitStream, got {other:?}"),
                };
                seen_tx.send(req).unwrap();
                ControlServerMsg::Response {
                    req_id,
                    reply: ControlReply::Ok(ReplyOk::Unit),
                }
                .encode(&mut sock)
                .unwrap();
                for bytes in [b"alpha\nbe".to_vec(), b"ta\ngamma\n".to_vec()] {
                    ControlServerMsg::Event(ControlEvent::GitChunk { id, bytes })
                        .encode(&mut sock)
                        .unwrap();
                }
                ControlServerMsg::Event(ControlEvent::GitEnd {
                    id,
                    code: Some(0),
                    failed: false,
                })
                .encode(&mut sock)
                .unwrap();
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

    fn scripted(
        steps: Vec<Result<GitStreamMsg, mpsc::RecvTimeoutError>>,
    ) -> impl FnMut(Duration) -> Result<GitStreamMsg, mpsc::RecvTimeoutError> {
        let mut steps = steps.into_iter();
        move |_| {
            steps
                .next()
                .unwrap_or(Err(mpsc::RecvTimeoutError::Disconnected))
        }
    }

    #[test]
    fn a_stream_that_goes_silent_times_out() {
        let mut lines = Vec::new();
        let queued = AtomicUsize::new(b"alpha\n".len());
        let mut next = scripted(vec![
            Ok(GitStreamMsg::Chunk(b"alpha\n".to_vec())),
            Err(mpsc::RecvTimeoutError::Timeout),
        ]);
        let got = drain_git_stream(&mut next, &queued, Duration::from_millis(120), &mut |l| {
            lines.push(l.to_string())
        });

        let err = got.expect_err("a stream that never ends must not read as success");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert_eq!(lines, ["alpha"], "what did arrive was still delivered");
    }

    #[test]
    fn the_idle_window_is_the_gap_between_messages_not_the_whole_stream() {
        let mut steps: Vec<Result<GitStreamMsg, mpsc::RecvTimeoutError>> = (0..5)
            .map(|i| Ok(GitStreamMsg::Chunk(format!("line {i}\n").into_bytes())))
            .collect();
        steps.push(Ok(GitStreamMsg::End {
            code: Some(0),
            failed: false,
        }));

        let mut lines = Vec::new();
        let queued = AtomicUsize::new(0);
        let mut next = scripted(steps);
        let code = drain_git_stream(&mut next, &queued, Duration::from_millis(150), &mut |l| {
            lines.push(l.to_string())
        })
        .unwrap();

        assert_eq!(code, Some(0));
        assert_eq!(
            lines.len(),
            5,
            "every message reset the window, so none of them was cut: {lines:?}"
        );
    }

    #[test]
    fn a_stream_that_outruns_its_queue_budget_is_cut_loose() {
        let registry = GitStreamRegistry::default();
        let (tx, rx) = mpsc::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        registry.insert(
            1,
            StreamSink {
                tx,
                queued: Arc::clone(&queued),
            },
        );

        let chunk = vec![b'x'; 1024 * 1024];
        let pushes = GIT_STREAM_QUEUE_BUDGET / chunk.len() + 2;
        for _ in 0..pushes {
            registry.dispatch(ControlEvent::GitChunk {
                id: 1,
                bytes: chunk.clone(),
            });
        }
        assert!(
            queued.load(Ordering::Acquire) <= GIT_STREAM_QUEUE_BUDGET + chunk.len(),
            "the arrears stopped growing at the budget, not at the diff's size"
        );

        registry.dispatch(ControlEvent::GitChunk {
            id: 1,
            bytes: chunk.clone(),
        });

        let mut lines = Vec::new();
        let err = drain_git_stream(
            &mut |d| rx.recv_timeout(d),
            &queued,
            Duration::from_secs(5),
            &mut |l| lines.push(l.to_string()),
        )
        .expect_err("a cut-off stream must not read as a complete diff");
        assert!(err.to_string().contains("outran"), "{err}");
    }

    #[test]
    fn a_large_but_drained_stream_never_trips_the_budget() {
        let registry = Arc::new(GitStreamRegistry::default());
        let (tx, rx) = mpsc::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        registry.insert(
            1,
            StreamSink {
                tx,
                queued: Arc::clone(&queued),
            },
        );

        let feeder = Arc::clone(&registry);
        let feeder_queued = Arc::clone(&queued);
        let chunks = GIT_STREAM_QUEUE_BUDGET / (1024 * 1024) * 2;
        thread::spawn(move || {
            for _ in 0..chunks {
                let deadline = Instant::now() + Duration::from_secs(10);
                while feeder_queued.load(Ordering::Acquire) > 4 * 1024 * 1024 {
                    if Instant::now() > deadline {
                        break;
                    }
                    thread::yield_now();
                }
                let mut bytes = vec![b'x'; 1024 * 1024 - 1];
                bytes.push(b'\n');
                feeder.dispatch(ControlEvent::GitChunk { id: 1, bytes });
            }
            feeder.dispatch(ControlEvent::GitEnd {
                id: 1,
                code: Some(0),
                failed: false,
            });
        });

        let mut lines = 0usize;
        let code = drain_git_stream(
            &mut |d| rx.recv_timeout(d),
            &queued,
            Duration::from_secs(10),
            &mut |_| lines += 1,
        )
        .expect("a drained stream is not an overrun, however much crosses it");
        assert_eq!(code, Some(0));
        assert_eq!(lines, chunks, "every line arrived");
        assert_eq!(queued.load(Ordering::Acquire), 0, "the arrears settled");
    }

    #[test]
    fn a_link_that_dies_mid_stream_ends_the_read() {
        let host = host_with_peer_dying_mid_stream();
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let got = host.git_lines(Path::new("/repo"), &["diff"], &mut |_| {});
            let _ = done_tx.send(got.is_err());
        });

        match done_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(errored) => assert!(
                errored,
                "a stream cut short must report an error, not a successful read of half a diff"
            ),
            Err(_) => panic!(
                "git_lines never returned: the reader is parked on a stream nothing will close"
            ),
        }
    }

    fn host_with_peer_dying_mid_stream() -> Arc<RemoteHost> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Hello(_) => {}
                other => panic!("expected Hello, got {other:?}"),
            }
            ControlServerMsg::HelloOk(hello_ok('/'))
                .encode(&mut sock)
                .unwrap();
            sock.flush().unwrap();
            let (req_id, id) = match ControlClientMsg::read(&mut sock) {
                Ok(ControlClientMsg::Request {
                    req_id,
                    req: ControlRequest::GitStream { id, .. },
                }) => (req_id, id),
                other => panic!("expected a GitStream request, got {other:?}"),
            };
            ControlServerMsg::Response {
                req_id,
                reply: ControlReply::Ok(ReplyOk::Unit),
            }
            .encode(&mut sock)
            .unwrap();
            ControlServerMsg::Event(ControlEvent::GitChunk {
                id,
                bytes: b"alpha\n".to_vec(),
            })
            .encode(&mut sock)
            .unwrap();
            sock.flush().unwrap();
        });

        let sock = TcpStream::connect(addr).unwrap();
        RemoteHost::over_tcp(
            sock,
            "ssh-alias:testbox",
            &ControlHello::host_rpc("tok", "laptop"),
        )
        .unwrap()
    }

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
        assert!(matches!(sent[2], ControlRequest::Exists { .. }));
        assert!(matches!(sent[3], ControlRequest::Canonicalize { .. }));
        assert!(matches!(sent[4], ControlRequest::RepoRoot { .. }));
        assert!(matches!(sent[5], ControlRequest::Search { .. }));
        assert!(matches!(sent[6], ControlRequest::Git { .. }));
    }

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

    #[test]
    fn the_host_id_is_derived_from_the_connection_and_is_stable() {
        let (a, _sa) = host_with_peer('/', |_| Some((ControlReply::Ok(ReplyOk::Unit), vec![])));
        let (b, _sb) = host_with_peer('/', |_| Some((ControlReply::Ok(ReplyOk::Unit), vec![])));
        assert_eq!(a.id(), b.id(), "same connection key, same id");
        assert_eq!(a.id(), a.id());
        assert_ne!(a.id(), HostId::LOCAL, "a remote host is never the local id");
        assert_eq!(a.id(), HostId::from_connection_key("ssh-alias:testbox"));
    }

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

        host.watches.dispatch(ControlEvent::Watch {
            id: 7,
            paths: vec!["/p/a".into(), "/p/b".into()],
        });
        assert_eq!(
            sub.events().recv_blocking().unwrap(),
            vec![PathBuf::from("/p/a"), PathBuf::from("/p/b")]
        );

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

        host.watches.dispatch(ControlEvent::WatchOverflow { id: 7 });
        assert_eq!(
            sub.events().recv_blocking().unwrap(),
            vec![PathBuf::from("/p"), PathBuf::from("/q")],
            "overflow must name the updated set, not the one watch() opened with"
        );
    }

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

    #[test]
    fn a_dead_link_reports_disconnected() {
        let (host, _seen) = host_with_peer('/', |_| None);
        let h: &dyn Host = host.as_ref();
        assert!(h.is_connected());
        let e = h.stat(Path::new("/f")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionReset);
        assert!(!h.is_connected());
    }

    #[test]
    fn a_reply_of_the_wrong_shape_is_invalid_data() {
        let (host, _seen) =
            host_with_peer('/', |_| Some((ControlReply::Ok(ReplyOk::Pong), vec![])));
        let e = host.stat(Path::new("/f")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("Pong"));
    }

    #[test]
    fn remote_host_is_object_safe() {
        let (host, _seen) =
            host_with_peer('/', |_| Some((ControlReply::Ok(ReplyOk::Unit), vec![])));
        let shared: SharedHost = host.into_shared();
        assert_eq!(shared.separator(), '/');
    }
}

//! One running language server: its process, the threads that pump its
//! pipes, and the request/response bookkeeping on top of [`super::rpc`].
//!
//! Each pipe gets a dedicated thread rather than a task on gpui's background
//! executor: a read that blocks until the server says something would park
//! one of the executor's few workers for the life of the server. The threads
//! end on their own when the pipes close, which killing the process does.
//!
//! What the server sends unprompted — diagnostics, its own requests, the pipe
//! closing — arrives on the [`ServerEvent`] channel, for the store to handle
//! on the main thread.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use serde_json::Value;

use super::rpc::{self, FrameReader, Incoming, RequestId, ResponseError};

type Reply = smol::channel::Sender<Result<Value, ResponseError>>;
/// Requests waiting for an answer. `None` once the server's output has
/// closed: nothing will ever be answered again, and a request made after
/// that fails at once instead of waiting forever.
type Pending = Arc<Mutex<Option<HashMap<RequestId, Reply>>>>;

/// How long an ordinary request waits for its answer. Everything the editor
/// asks is interactive — someone is waiting on a completion, a jump, a
/// rename — and a server that has not answered in this long is stuck.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// `initialize` may index before answering; give it longer.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);

/// Something the server did that nobody asked for.
#[derive(Debug)]
pub(crate) enum ServerEvent {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// The server's output closed: it exited, crashed or was killed.
    Exited,
}

pub(crate) struct LspClient {
    next_id: AtomicI64,
    pending: Pending,
    out: std::sync::mpsc::Sender<Vec<u8>>,
    /// Messages held back until the server has answered `initialize`, which
    /// the protocol requires to be the first thing it sees. `None` once the
    /// gate is open.
    gate: Mutex<Option<Vec<Vec<u8>>>>,
    child: Mutex<Option<Child>>,
}

impl LspClient {
    /// Starts `program` in `root` and wires up its pipes.
    pub(crate) fn spawn(
        name: &str,
        program: &Path,
        args: &[&str],
        root: &Path,
    ) -> anyhow::Result<(Arc<Self>, smol::channel::Receiver<ServerEvent>)> {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            // CREATE_NO_WINDOW: a server is a pipe, not a console.
            command.creation_flags(0x0800_0000);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        if let Some(stderr) = child.stderr.take() {
            let name = name.to_owned();
            // A server that fills its stderr pipe with nobody reading blocks
            // on the next write — and stops answering. Drained into the log.
            std::thread::Builder::new()
                .name("tty7-lsp-stderr".into())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines() {
                        let Ok(line) = line else { break };
                        log::debug!("lsp {name} stderr: {line}");
                    }
                })
                .ok();
        }
        let (client, events) = Self::from_streams(name, stdout, stdin)?;
        *client.child.lock().unwrap() = Some(child);
        Ok((client, events))
    }

    /// The client over any pair of pipes — a real process's, or a test's.
    pub(crate) fn from_streams(
        name: &str,
        reader: impl Read + Send + 'static,
        mut writer: impl Write + Send + 'static,
    ) -> anyhow::Result<(Arc<Self>, smol::channel::Receiver<ServerEvent>)> {
        let (out, out_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name("tty7-lsp-write".into())
            .spawn(move || {
                // Ends when every sender is gone — the client dropped — or
                // the pipe breaks. Either way stdin closes behind it, which
                // is the last word a server needs to exit.
                while let Ok(bytes) = out_rx.recv() {
                    if writer
                        .write_all(&bytes)
                        .and_then(|_| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            })?;

        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (events_tx, events) = smol::channel::unbounded();
        {
            let pending = pending.clone();
            let name = name.to_owned();
            let mut reader = reader;
            std::thread::Builder::new()
                .name("tty7-lsp-read".into())
                .spawn(move || {
                    let mut frames = FrameReader::default();
                    let mut chunk = vec![0u8; 64 * 1024];
                    loop {
                        let n = match reader.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        frames.push(&chunk[..n]);
                        while let Some(body) = frames.next_frame() {
                            let Some(message) = rpc::classify(&body) else {
                                log::debug!("lsp {name}: ignoring an unreadable message");
                                continue;
                            };
                            let event = match message {
                                Incoming::Response { id, result } => {
                                    let reply = pending
                                        .lock()
                                        .unwrap()
                                        .as_mut()
                                        .and_then(|waiting| waiting.remove(&id));
                                    if let Some(reply) = reply {
                                        let _ = reply.try_send(result);
                                    }
                                    continue;
                                }
                                Incoming::Request { id, method, params } => {
                                    ServerEvent::Request { id, method, params }
                                }
                                Incoming::Notification { method, params } => {
                                    ServerEvent::Notification { method, params }
                                }
                            };
                            if events_tx.send_blocking(event).is_err() {
                                // Nobody is listening any more; keep reading
                                // so the server never blocks on a full pipe.
                            }
                        }
                    }
                    // Every request still waiting will never be answered:
                    // dropping the senders fails them rather than leaving
                    // them hanging.
                    pending.lock().unwrap().take();
                    let _ = events_tx.send_blocking(ServerEvent::Exited);
                })?;
        }

        Ok((
            Arc::new(Self {
                next_id: AtomicI64::new(1),
                pending,
                out,
                gate: Mutex::new(Some(Vec::new())),
                child: Mutex::new(None),
            }),
            events,
        ))
    }

    fn send(&self, bytes: Vec<u8>, gated: bool) {
        let mut gate = self.gate.lock().unwrap();
        match gate.as_mut() {
            Some(held) if gated => held.push(bytes),
            _ => {
                let _ = self.out.send(bytes);
            }
        }
    }

    /// Lets through everything held back for `initialize`, in the order it
    /// was sent. Held under the gate's lock, so nothing sent meanwhile can
    /// overtake it.
    pub(crate) fn open_gate(&self) {
        let mut gate = self.gate.lock().unwrap();
        for bytes in gate.take().unwrap_or_default() {
            let _ = self.out.send(bytes);
        }
    }

    /// Sends a request, returning the server's answer. Dropping the future
    /// before it resolves tells the server to stop working on it.
    pub(crate) fn request_raw(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> impl std::future::Future<Output = anyhow::Result<Value>> + use<> {
        self.request_inner(method, params, true, REQUEST_TIMEOUT)
    }

    /// [`Self::request_raw`] with its own time limit.
    pub(crate) fn request_raw_within(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> impl std::future::Future<Output = anyhow::Result<Value>> + use<> {
        self.request_inner(method, params, true, timeout)
    }

    /// `initialize`, which is the one request that goes past the gate.
    pub(crate) fn initialize(
        self: &Arc<Self>,
        params: Value,
    ) -> impl std::future::Future<Output = anyhow::Result<Value>> + use<> {
        self.request_inner("initialize", params, false, INITIALIZE_TIMEOUT)
    }

    fn request_inner(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        gated: bool,
        timeout: Duration,
    ) -> impl std::future::Future<Output = anyhow::Result<Value>> + use<> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = smol::channel::bounded(1);
        // Checked and registered under one lock: the reader closes the map
        // under the same lock, so a request either makes it in before the
        // server went away (and is failed with the rest) or sees it gone.
        let registered = match self.pending.lock().unwrap().as_mut() {
            Some(waiting) => {
                waiting.insert(id, tx);
                true
            }
            None => false,
        };
        if registered {
            self.send(rpc::encode(&rpc::request(id, method, params)), gated);
        }
        let guard = CancelOnDrop {
            client: Arc::downgrade(self),
            id,
            done: !registered,
        };
        let method = method.to_owned();
        async move {
            // Bound whole, not just the field written below: a closure
            // capturing only `guard.done` would drop the guard — and cancel
            // the request — before it was ever sent an answer.
            let mut guard = guard;
            if !registered {
                return Err(anyhow!("{method}: the language server has exited"));
            }
            let answer = smol::future::or(async { Some(rx.recv().await) }, async {
                smol::Timer::after(timeout).await;
                None
            })
            .await;
            // A timeout leaves `done` unset, so dropping the guard tells the
            // server to stop working on it.
            let Some(answer) = answer else {
                return Err(anyhow!("{method}: no answer in {}s", timeout.as_secs_f32()));
            };
            guard.done = true;
            match answer {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(e)) => Err(anyhow::Error::new(e).context(method)),
                Err(_) => Err(anyhow!("{method}: the language server went away")),
            }
        }
    }

    /// A typed request.
    pub(crate) fn request<R: lsp_types::request::Request>(
        self: &Arc<Self>,
        params: R::Params,
    ) -> impl std::future::Future<Output = anyhow::Result<R::Result>> + use<R> {
        let params = serde_json::to_value(params).unwrap_or(Value::Null);
        let answer = self.request_raw(R::METHOD, params);
        async move { Ok(serde_json::from_value(answer.await?)?) }
    }

    pub(crate) fn notify_raw(&self, method: &str, params: Value) {
        self.send(rpc::encode(&rpc::notification(method, params)), true);
    }

    pub(crate) fn notify<N: lsp_types::notification::Notification>(&self, params: N::Params) {
        let params = serde_json::to_value(params).unwrap_or(Value::Null);
        self.notify_raw(N::METHOD, params);
    }

    /// Answers a request the server made.
    pub(crate) fn respond(&self, id: Value, result: Result<Value, ResponseError>) {
        self.send(rpc::encode(&rpc::response(id, result)), false);
    }

    /// Kills the process, if there is one and it is still running, and reaps
    /// it so it does not linger as a zombie.
    pub(crate) fn kill(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }

    /// Whether the process has already exited on its own.
    pub(crate) fn has_exited(&self) -> bool {
        match self.child.lock().unwrap().as_mut() {
            Some(child) => !matches!(child.try_wait(), Ok(None)),
            None => true,
        }
    }

    /// Reaps the process if it has exited, without killing it.
    pub(crate) fn reap_if_exited(&self) -> bool {
        let mut slot = self.child.lock().unwrap();
        let exited = match slot.as_mut() {
            Some(child) => !matches!(child.try_wait(), Ok(None)),
            None => return true,
        };
        if exited {
            slot.take();
        }
        exited
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // The last line of defence against a server outliving tty7's
        // interest in it. A clean shutdown has already reaped it.
        self.kill();
    }
}

struct CancelOnDrop {
    client: Weak<LspClient>,
    id: RequestId,
    done: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let Some(client) = self.client.upgrade() else {
            return;
        };
        let removed = client
            .pending
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|waiting| waiting.remove(&self.id));
        if removed.is_some() {
            client.notify_raw("$/cancelRequest", serde_json::json!({ "id": self.id }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A server on the other end of two pipes, driven by the test.
    struct FakeServer {
        frames: FrameReader,
        from_client: std::io::PipeReader,
        to_client: std::io::PipeWriter,
    }

    impl FakeServer {
        fn next(&mut self) -> Value {
            let mut chunk = [0u8; 256];
            loop {
                if let Some(body) = self.frames.next_frame() {
                    return serde_json::from_slice(&body).unwrap();
                }
                let n = self.from_client.read(&mut chunk).unwrap();
                assert!(n > 0, "the client closed its end");
                self.frames.push(&chunk[..n]);
            }
        }

        fn send(&mut self, value: Value) {
            // Written in two pieces, so the client's reader has to put a
            // frame back together.
            let bytes = rpc::encode(&value);
            let (a, b) = bytes.split_at(bytes.len() / 2);
            self.to_client.write_all(a).unwrap();
            self.to_client.flush().unwrap();
            self.to_client.write_all(b).unwrap();
            self.to_client.flush().unwrap();
        }
    }

    fn connect() -> (
        Arc<LspClient>,
        smol::channel::Receiver<ServerEvent>,
        FakeServer,
    ) {
        let (client_reads, server_writes) = std::io::pipe().unwrap();
        let (server_reads, client_writes) = std::io::pipe().unwrap();
        let (client, events) =
            LspClient::from_streams("fake", client_reads, client_writes).unwrap();
        (
            client,
            events,
            FakeServer {
                frames: FrameReader::default(),
                from_client: server_reads,
                to_client: server_writes,
            },
        )
    }

    #[test]
    fn nothing_but_initialize_reaches_the_server_until_the_gate_opens() {
        let (client, _events, mut server) = connect();
        client.notify_raw("textDocument/didOpen", json!({ "n": 1 }));
        let init = client.initialize(json!({}));
        let first = server.next();
        assert_eq!(first["method"], "initialize");
        server
            .send(json!({ "jsonrpc": "2.0", "id": first["id"], "result": { "capabilities": {} } }));
        let answer = smol::block_on(init).unwrap();
        assert_eq!(answer, json!({ "capabilities": {} }));
        client.notify_raw("initialized", json!({}));
        client.open_gate();
        // Held messages come out first, in the order they were sent.
        assert_eq!(server.next()["method"], "textDocument/didOpen");
        assert_eq!(server.next()["method"], "initialized");
    }

    #[test]
    fn answers_find_their_way_back_to_the_request_that_asked() {
        let (client, _events, mut server) = connect();
        client.open_gate();
        let a = client.request_raw("a", json!(1));
        let b = client.request_raw("b", json!(2));
        let ra = server.next();
        let rb = server.next();
        // Answered out of order.
        server.send(json!({ "jsonrpc": "2.0", "id": rb["id"], "result": "B" }));
        server.send(json!({
            "jsonrpc": "2.0",
            "id": ra["id"],
            "error": { "code": -32800, "message": "cancelled" },
        }));
        assert_eq!(smol::block_on(b).unwrap(), json!("B"));
        let err = smol::block_on(a).unwrap_err();
        assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
    }

    #[test]
    fn server_requests_and_notifications_arrive_as_events_and_can_be_answered() {
        let (client, events, mut server) = connect();
        client.open_gate();
        server.send(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": "file:///a.rs", "diagnostics": [] },
        }));
        server.send(json!({
            "jsonrpc": "2.0",
            "id": "cfg-1",
            "method": "workspace/configuration",
            "params": { "items": [{}] },
        }));
        match smol::block_on(events.recv()).unwrap() {
            ServerEvent::Notification { method, .. } => {
                assert_eq!(method, "textDocument/publishDiagnostics")
            }
            other => panic!("{other:?}"),
        }
        let ServerEvent::Request { id, method, .. } = smol::block_on(events.recv()).unwrap() else {
            panic!("expected a request");
        };
        assert_eq!(method, "workspace/configuration");
        client.respond(id, Ok(json!([null])));
        let reply = server.next();
        assert_eq!(reply["id"], "cfg-1");
        assert_eq!(reply["result"], json!([null]));
    }

    #[test]
    fn a_closed_pipe_fails_waiting_requests_and_says_the_server_exited() {
        let (client, events, server) = connect();
        client.open_gate();
        let waiting = client.request_raw("slow", Value::Null);
        drop(server);
        assert!(smol::block_on(waiting).is_err());
        assert!(matches!(
            smol::block_on(events.recv()).unwrap(),
            ServerEvent::Exited
        ));
    }

    #[test]
    fn a_request_after_the_server_exited_fails_at_once() {
        let (client, events, server) = connect();
        client.open_gate();
        drop(server);
        assert!(matches!(
            smol::block_on(events.recv()).unwrap(),
            ServerEvent::Exited
        ));
        // No timeout involved: a limit of an hour would hang the test.
        let late =
            client.request_raw_within("textDocument/hover", Value::Null, Duration::from_secs(3600));
        let err = smol::block_on(late).unwrap_err();
        assert!(format!("{err:#}").contains("exited"), "{err:#}");
    }

    #[test]
    fn a_request_nobody_answers_times_out_and_is_cancelled() {
        let (client, _events, mut server) = connect();
        client.open_gate();
        let asked = client.request_raw_within("slow", Value::Null, Duration::from_millis(50));
        let sent = server.next();
        let err = smol::block_on(asked).unwrap_err();
        assert!(format!("{err:#}").contains("no answer"), "{err:#}");
        let cancel = server.next();
        assert_eq!(cancel["method"], "$/cancelRequest");
        assert_eq!(cancel["params"]["id"], sent["id"]);
    }

    #[test]
    fn dropping_an_unanswered_request_cancels_it() {
        let (client, _events, mut server) = connect();
        client.open_gate();
        let asked = client.request_raw("textDocument/completion", Value::Null);
        let sent = server.next();
        drop(asked);
        let cancel = server.next();
        assert_eq!(cancel["method"], "$/cancelRequest");
        assert_eq!(cancel["params"]["id"], sent["id"]);
    }
}

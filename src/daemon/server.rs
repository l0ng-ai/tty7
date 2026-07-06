//! Daemon server: the Unix-domain-socket listener, pane registry, and `--daemon`
//! entry point.
//!
//! One process hosts many panes ([`DaemonPane`]); one socket connection drives one
//! pane (matching the protocol's "one connection = one pane" model). The server:
//!   1. resolves the socket path under the (config-dir-aware) config directory,
//!      so `cargo dev` / `--config-dir` isolation reaches the daemon too;
//!   2. clears a *stale* socket (one that nothing is listening on) before binding;
//!   3. accepts connections, spawning a thread per connection.
//!
//! Per-connection flow (see [`handle_conn`]): read the first `ClientMsg`.
//!   - `Spawn` → create a pane, reply `Spawned`, attach this connection, stream.
//!   - `Attach` → look the pane up; on hit attach + stream, on miss reply `Error`.
//!   - `List`  → reply `PaneList`, then close.
//! While streaming, a small writer thread drains the pane's `DaemonMsg` channel to
//! the socket, while the main connection thread reads further client messages
//! (`Input` / `Resize` / `Detach` / `Kill`). Connection close == detach (the pane
//! keeps running headless).

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use crate::daemon::pane::DaemonPane;
use crate::daemon::protocol::{ClientMsg, DaemonMsg};
use crate::daemon::transport::{self, Stream};

/// Shared pane registry: id → pane, plus a monotonic id source.
struct Registry {
    panes: Mutex<HashMap<u64, Arc<DaemonPane>>>,
    next_id: AtomicU64,
}

impl Registry {
    fn new() -> Self {
        Self {
            panes: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn insert(&self, pane: Arc<DaemonPane>) {
        self.panes.lock().unwrap().insert(pane.id, pane);
    }

    fn get(&self, id: u64) -> Option<Arc<DaemonPane>> {
        self.panes.lock().unwrap().get(&id).cloned()
    }

    /// Remove a pane from the registry (its `Arc` drop hangs up + reaps the child
    /// once the last connection releases it).
    fn remove(&self, id: u64) -> Option<Arc<DaemonPane>> {
        self.panes.lock().unwrap().remove(&id)
    }

    /// Remove every pane and hang up its child. Used by the `Shutdown` control
    /// message right before the process exits: the children must be signalled now
    /// (SIGHUP → SIGKILL, via `pane.kill()`), or the exit would orphan them —
    /// reparented to launchd and still holding their PTYs — instead of ending the
    /// session cleanly. Drains under the lock, then kills with the lock released
    /// so a pane's teardown can't deadlock against the registry.
    fn drain_and_kill(&self) {
        let panes: Vec<Arc<DaemonPane>> = {
            let mut guard = self.panes.lock().unwrap();
            guard.drain().map(|(_, p)| p).collect()
        };
        for pane in panes {
            pane.kill();
        }
    }

    /// Snapshot of all panes' metadata for `List`.
    fn list(&self) -> Vec<crate::daemon::protocol::PaneInfo> {
        self.panes
            .lock()
            .unwrap()
            .values()
            .map(|p| p.info())
            .collect()
    }
}

/// Run the daemon: bind the socket and serve connections forever. Returns `Err`
/// only on a fatal setup failure (bad socket path, bind error); the accept loop
/// itself runs until the process is killed.
pub fn run() -> anyhow::Result<()> {
    // If an endpoint marker is already there, it's either a live daemon (we should
    // bail) or a stale leftover from a crash (we should clear it and take over).
    // Probe by connecting: success means someone's listening — don't double-run.
    if transport::endpoint_exists() {
        match transport::connect() {
            Ok(_) => {
                anyhow::bail!(
                    "daemon already running at {}",
                    transport::endpoint_display()
                );
            }
            Err(_) => {
                // Nothing listening: stale endpoint from a previous run. Clear it so
                // `bind` below can recreate it.
                transport::remove_stale_endpoint();
            }
        }
    }

    let listener = transport::bind()?;
    log::info!("daemon listening on {}", transport::endpoint_display());

    let registry = Arc::new(Registry::new());

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // Both directions get tuned: `transport::connect` covers the
                // GUI's end, this covers the daemon's (where the send buffer
                // carries the full output throughput).
                transport::tune(&stream);
                let registry = registry.clone();
                // One thread per connection; the connection owns its pane stream.
                std::thread::Builder::new()
                    .name("tty7-daemon-conn".to_string())
                    .spawn(move || {
                        // This thread relays client input (keystrokes) to the
                        // PTY: interactive by definition.
                        crate::core::threads::promote_to_user_interactive();
                        if let Err(e) = handle_conn(stream, registry) {
                            // A clean client disconnect surfaces as an EOF error; log
                            // at debug so it isn't noise.
                            log::debug!("connection ended: {e}");
                        }
                    })
                    .ok();
            }
            // A transient accept error shouldn't kill the daemon; log and continue.
            Err(e) => log::warn!("accept failed: {e}"),
        }
    }

    Ok(())
}

/// Handle one connection start-to-finish. Reads the opening `ClientMsg` and
/// dispatches; for the streaming variants it then runs [`stream_pane`].
fn handle_conn(stream: Stream, registry: Arc<Registry>) -> anyhow::Result<()> {
    let mut read_stream = stream;

    // Authenticate before touching the protocol. On Windows the transport is
    // loopback TCP, reachable by any local process; the client proves it read the
    // user-private port file by presenting the daemon's token as a preamble. A
    // failed check drops the connection here, before any `ClientMsg` is parsed or a
    // pane is spawned. No-op on Unix (the socket's filesystem perms already gate it).
    transport::authenticate(&mut read_stream)?;

    // Separate read/write halves so the writer thread and reader loop don't share a
    // `&mut` (the stream is just a socket; `try_clone` dups the handle — both
    // directions are independent).
    let write_stream = read_stream.try_clone()?;

    let first = ClientMsg::read(&mut read_stream)?;
    match first {
        ClientMsg::Spawn { cwd, size } => {
            let id = registry.alloc_id();
            // Reclaim a pane whose child exits while *detached* (nobody attached,
            // so no connection's detach path will ever drop it): remove it from
            // the registry, freeing the ring and reaping the zombie child. The
            // removal runs on its own short-lived thread because `on_dead` fires
            // on the pane's reader thread, and dropping the last `Arc` there
            // would make `DaemonPane::drop`'s reader join wait (bounded) on the
            // very thread it is running on.
            let on_dead = {
                let registry = registry.clone();
                move || {
                    std::thread::Builder::new()
                        .name("tty7-daemon-pane-reap".to_string())
                        .spawn(move || {
                            registry.remove(id);
                        })
                        .ok();
                }
            };
            let pane = match DaemonPane::spawn(id, cwd, size, on_dead) {
                Ok(p) => p,
                Err(e) => {
                    // Report the failure to the client and close.
                    let mut w = write_stream;
                    let _ = DaemonMsg::Error(format!("spawn failed: {e}")).encode(&mut w);
                    return Err(e);
                }
            };
            registry.insert(pane.clone());

            // Reply with the new id, then attach this connection and stream.
            {
                let mut w = &write_stream;
                DaemonMsg::Spawned { pane_id: id }.encode(&mut w)?;
            }
            stream_pane(pane, id, read_stream, write_stream, registry)
        }

        // The attach `size` is the client's pre-layout placeholder and is
        // deliberately ignored: the daemon reports the recorded geometry via
        // `DaemonMsg::Size` for the replay, and the client sends a real
        // `Resize` once laid out (see `DaemonPane::attach`).
        ClientMsg::Attach { pane_id, size: _ } => match registry.get(pane_id) {
            Some(pane) => {
                stream_pane_with_attach(pane, pane_id, read_stream, write_stream, registry)
            }
            None => {
                let mut w = write_stream;
                DaemonMsg::Error(format!("no such pane {pane_id}")).encode(&mut w)?;
                Ok(())
            }
        },

        ClientMsg::List => {
            let mut w = write_stream;
            DaemonMsg::PaneList(registry.list()).encode(&mut w)?;
            Ok(())
        }

        ClientMsg::Shutdown => {
            // Force a full daemon stop (the GUI's "Restart Background Service"):
            // hang up every child so nothing is orphaned, drop the endpoint
            // marker so a fresh daemon binds cleanly, then exit. The accept loop
            // has no cooperative stop — a hard exit *is* the daemon's defined stop
            // (see `run`'s "runs until the process is killed"). This is the one
            // place the daemon terminates itself.
            log::info!("daemon shutting down on client request");
            registry.drain_and_kill();
            transport::remove_stale_endpoint();
            std::process::exit(0);
        }

        ClientMsg::Kill { pane_id } => {
            // A control-only `Kill` as the opening message: terminate + forget the
            // pane, then close (no stream).
            if let Some(pane) = registry.remove(pane_id) {
                pane.kill();
            }
            Ok(())
        }

        // `Input` / `Resize` / `Detach` as an opening message are meaningless (no
        // pane is bound yet); ignore and close.
        other => {
            log::debug!("unexpected opening message: {other:?}");
            Ok(())
        }
    }
}

/// `Attach` path: subscribe the connection to an existing pane (sending the
/// recorded `Size` + `Snapshot` + known cwd/prompt), then stream. Splitting
/// this out keeps the `Spawn` path (which mustn't re-snapshot before its
/// `Spawned` reply ordering) distinct from `Attach`.
fn stream_pane_with_attach(
    pane: Arc<DaemonPane>,
    id: u64,
    read_stream: Stream,
    write_stream: Stream,
    registry: Arc<Registry>,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<DaemonMsg>();
    let epoch = pane.attach(tx);
    run_stream(pane, id, epoch, rx, read_stream, write_stream, registry)
}

/// `Spawn` path: the pane was just created (empty ring), so attaching now sends an
/// empty `Snapshot` (plus the spawn geometry as `Size`) — harmless, and it keeps
/// the single attach code path. The `Spawned` reply has already been written by
/// the caller.
fn stream_pane(
    pane: Arc<DaemonPane>,
    id: u64,
    read_stream: Stream,
    write_stream: Stream,
    registry: Arc<Registry>,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel::<DaemonMsg>();
    let epoch = pane.attach(tx);
    run_stream(pane, id, epoch, rx, read_stream, write_stream, registry)
}

/// Drive the bidirectional stream for an attached pane:
///   - a writer thread drains the pane→client `DaemonMsg` channel to the socket;
///   - this thread reads further `ClientMsg`s (`Input` / `Resize` / `Detach` /
///     `Kill`) until the client disconnects or detaches.
/// On exit we detach (never kill — the pane lives on headless) unless the client
/// explicitly asked to `Kill`.
fn run_stream(
    pane: Arc<DaemonPane>,
    id: u64,
    epoch: u64,
    rx: Receiver<DaemonMsg>,
    mut read_stream: Stream,
    write_stream: Stream,
    registry: Arc<Registry>,
) -> anyhow::Result<()> {
    // Writer thread: pull daemon messages and frame them onto the socket. It ends
    // when the channel's senders are all dropped (pane detached / replaced) or a
    // socket write fails (client gone).
    let writer = spawn_writer(rx, write_stream, pane.gate());

    // Reader loop: process client→daemon messages until disconnect/detach.
    let mut killed = false;
    loop {
        match ClientMsg::read(&mut read_stream) {
            Ok(ClientMsg::Input(bytes)) => pane.write_input(&bytes),
            Ok(ClientMsg::Resize(size)) => pane.resize(size),
            Ok(ClientMsg::Detach) => break,
            Ok(ClientMsg::Kill { pane_id }) => {
                // Honor a kill for *this* pane; for another id, just remove+kill it
                // and keep streaming this one.
                if pane_id == id {
                    killed = true;
                    break;
                } else if let Some(other) = registry.remove(pane_id) {
                    other.kill();
                }
            }
            // Re-`Attach` / `Spawn` / `List` mid-stream aren't part of v1's single
            // connection-per-pane model; ignore.
            Ok(_) => {}
            // EOF / error == the client went away: detach.
            Err(_) => break,
        }
    }

    // Detach this connection from the pane so its reader stops sending to our
    // channel. `detach` drops the pane's `Sender`; with no senders left, the
    // writer thread's `rx.recv()` returns `Err` and it exits on its own. Join it so
    // the socket fd it holds is released before we return. `detach` also reports
    // whether the pane is now reclaimable (child already exited + no subscriber).
    let reclaimable = pane.detach(epoch);
    let _ = writer.join();

    if killed {
        if let Some(p) = registry.remove(id) {
            p.kill();
        }
    } else if reclaimable {
        // The shell exited while we were attached; now that the last client is
        // leaving, drop the dead pane instead of leaving it (and its ~8 MiB ring,
        // PTY fds, and unreaped child) in the registry forever. A `!alive` pane is
        // never re-attached — clients spawn fresh for it — so this is invisible to
        // them. The `Arc` we still hold reaps the child when this frame returns.
        registry.remove(id);
    }
    Ok(())
}

/// While coalescing, stop growing a merged `Output` frame past this size. Big
/// enough to turn a flood's ~1 KiB PTY reads into a few large frames per client
/// wake, small enough to keep any single socket write (and the client's
/// apply-under-lock for it) bounded.
const OUTPUT_COALESCE_CAP: usize = 256 * 1024;

/// Spawn the per-connection writer thread that frames pane `DaemonMsg`s onto the
/// socket. The thread self-terminates when its channel closes (all senders dropped
/// — i.e. the pane detached us) or a socket write fails (client gone).
///
/// Consecutive `Output` messages already queued are merged into one frame (up to
/// [`OUTPUT_COALESCE_CAP`]) before encoding. macOS PTYs hand the pane reader
/// ~1 KiB per read, so a flood otherwise becomes thousands of tiny frames per
/// second, and the *client* pays per frame (term lock + parser call + wakeup);
/// merging here collapses that to a handful of large frames. Only what is
/// already in the channel is drained — `try_recv` never waits — so a lone
/// keystroke echo still goes out immediately, and ordering with non-`Output`
/// messages (Cwd/Prompt/Exited…) is preserved.
fn spawn_writer(
    rx: Receiver<DaemonMsg>,
    mut write_stream: Stream,
    gate: Arc<crate::daemon::pane::OutputGate>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("tty7-daemon-writer".to_string())
        .spawn(move || {
            // On the visible-output path (PTY reader → here → client socket):
            // keep it off the efficiency cores.
            crate::core::threads::promote_to_user_interactive();
            // A non-Output message that interrupted a coalescing run, waiting
            // its turn behind the merged frame it arrived after.
            let mut carried: Option<DaemonMsg> = None;
            loop {
                let msg = match carried.take() {
                    Some(m) => m,
                    // Block on the channel until the next message (or close).
                    None => match rx.recv() {
                        Ok(m) => m,
                        Err(_) => break,
                    },
                };
                let msg = if let DaemonMsg::Output(mut buf) = msg {
                    while buf.len() < OUTPUT_COALESCE_CAP {
                        match rx.try_recv() {
                            Ok(DaemonMsg::Output(more)) => buf.extend_from_slice(&more),
                            // A different message ends the run; it must be
                            // written *after* the bytes that preceded it.
                            Ok(other) => {
                                carried = Some(other);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    DaemonMsg::Output(buf)
                } else {
                    msg
                };
                // Credit the gate whether the write succeeds or not: either
                // way the bytes leave the queue, and the reader must not stay
                // throttled against them.
                let drained = match &msg {
                    DaemonMsg::Output(b) => b.len(),
                    _ => 0,
                };
                let write_ok = msg.encode(&mut write_stream).is_ok();
                if drained > 0 {
                    gate.sub(drained);
                }
                if !write_ok {
                    break;
                }
                // Flush so interactive output isn't held in a buffer (socket
                // writes are unbuffered, but be explicit/future-proof).
                let _ = write_stream.flush();
            }
        })
        .expect("spawn daemon writer thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_id_is_monotonic_from_one() {
        let reg = Registry::new();
        assert_eq!(reg.alloc_id(), 1);
        assert_eq!(reg.alloc_id(), 2);
        assert_eq!(reg.alloc_id(), 3);
    }

    #[test]
    fn empty_registry_get_remove_list_are_empty() {
        let reg = Registry::new();
        assert!(reg.get(1).is_none());
        assert!(reg.remove(1).is_none());
        assert!(reg.list().is_empty());
    }

    // The connection-dispatch tests drive `handle_conn` over a real socket pair,
    // exercising only the branches that need no PTY (List / Attach-miss / Kill /
    // unexpected-open) plus the writer thread. Unix-only: the Windows transport is
    // loopback TCP, which has no `pair()` helper.
    #[cfg(unix)]
    mod conn {
        use super::super::{OUTPUT_COALESCE_CAP, Registry, handle_conn, spawn_writer};
        use crate::daemon::protocol::{ClientMsg, DaemonMsg, WinSize};
        use std::os::unix::net::UnixStream;
        use std::sync::{Arc, mpsc};
        use std::thread;

        const SIZE: WinSize = WinSize {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        };

        /// Run `handle_conn` on the server end of a socket pair; hand back the client
        /// end plus the server thread's join handle.
        fn serve() -> (UnixStream, thread::JoinHandle<()>) {
            let (client, server) = UnixStream::pair().unwrap();
            let reg = Arc::new(Registry::new());
            let h = thread::spawn(move || {
                let _ = handle_conn(server, reg);
            });
            (client, h)
        }

        #[test]
        fn list_on_empty_registry_replies_with_empty_pane_list() {
            let (mut client, h) = serve();
            ClientMsg::List.encode(&mut client).unwrap();
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::PaneList(vec![])
            );
            h.join().unwrap();
        }

        #[test]
        fn attach_to_missing_pane_reports_error() {
            let (mut client, h) = serve();
            ClientMsg::Attach {
                pane_id: 999,
                size: SIZE,
            }
            .encode(&mut client)
            .unwrap();
            match DaemonMsg::read(&mut client).unwrap() {
                DaemonMsg::Error(msg) => assert!(msg.contains("999"), "error names the id"),
                other => panic!("expected Error, got {other:?}"),
            }
            h.join().unwrap();
        }

        #[test]
        fn kill_unknown_pane_closes_without_reply() {
            let (mut client, h) = serve();
            ClientMsg::Kill { pane_id: 123 }
                .encode(&mut client)
                .unwrap();
            // Kill as the opening message produces no reply — the server just closes.
            assert!(DaemonMsg::read(&mut client).is_err());
            h.join().unwrap();
        }

        #[test]
        fn unexpected_opening_message_is_ignored_and_closed() {
            let (mut client, h) = serve();
            // A `Resize` with no pane bound is meaningless; the server closes cleanly.
            ClientMsg::Resize(SIZE).encode(&mut client).unwrap();
            assert!(DaemonMsg::read(&mut client).is_err());
            h.join().unwrap();
        }

        #[test]
        fn spawn_writer_frames_messages_then_exits_on_channel_close() {
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(rx, server, Arc::new(crate::daemon::pane::OutputGate::new()));

            tx.send(DaemonMsg::Output(b"hi".to_vec())).unwrap();
            tx.send(DaemonMsg::Exited { code: Some(0) }).unwrap();
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Output(b"hi".to_vec())
            );
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Exited { code: Some(0) }
            );

            // Dropping the last sender ends the writer thread on its own.
            drop(tx);
            writer.join().unwrap();
        }

        /// Consecutive `Output`s already sitting in the channel leave the socket
        /// as a *single* merged frame with their bytes concatenated in order —
        /// the coalescing that collapses a flood's thousands of ~1 KiB PTY reads
        /// into a few large frames. The messages are queued (and the sender
        /// dropped) *before* the writer spawns, so all three are guaranteed
        /// visible inside one `recv` + `try_recv` window; a regression back to
        /// frame-per-message would deliver `Output("one")` first and fail the
        /// first assertion.
        #[test]
        fn spawn_writer_coalesces_queued_outputs_into_one_frame() {
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            tx.send(DaemonMsg::Output(b"one".to_vec())).unwrap();
            tx.send(DaemonMsg::Output(b"two".to_vec())).unwrap();
            tx.send(DaemonMsg::Output(b"three".to_vec())).unwrap();
            // Close the channel up front: the writer drains the backlog and then
            // exits, so the EOF below proves nothing trailed the merged frame.
            drop(tx);

            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(rx, server, Arc::new(crate::daemon::pane::OutputGate::new()));

            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Output(b"onetwothree".to_vec()),
                "queued Outputs must merge into one frame, bytes in send order"
            );
            // EOF, not another frame: the three messages became exactly one.
            assert!(DaemonMsg::read(&mut client).is_err());
            writer.join().unwrap();
        }

        /// A queued `Output` backlog larger than `OUTPUT_COALESCE_CAP` is split
        /// into multiple frames — every byte delivered, in order — rather than
        /// merged into one unbounded write. The cap is checked before each
        /// append, so a frame may overshoot it by at most one message; anything
        /// bigger means the cap stopped bounding socket writes (and the
        /// client's apply-under-lock per frame).
        #[test]
        fn spawn_writer_splits_output_backlog_at_the_coalesce_cap() {
            // Six 64 KiB chunks: 384 KiB total against the 256 KiB cap. Each is
            // filled with a distinct byte so the concatenation check below also
            // proves the split kept the chunks in order.
            const CHUNK: usize = 64 * 1024;
            let chunks: Vec<Vec<u8>> = (0u8..6).map(|i| vec![i; CHUNK]).collect();
            let expected: Vec<u8> = chunks.concat();

            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            for chunk in &chunks {
                tx.send(DaemonMsg::Output(chunk.clone())).unwrap();
            }
            drop(tx);

            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(rx, server, Arc::new(crate::daemon::pane::OutputGate::new()));

            let mut frames: Vec<Vec<u8>> = Vec::new();
            loop {
                match DaemonMsg::read(&mut client) {
                    Ok(DaemonMsg::Output(bytes)) => frames.push(bytes),
                    Ok(other) => panic!("expected only Output frames, got {other:?}"),
                    // EOF: the writer drained the backlog and exited.
                    Err(_) => break,
                }
            }
            writer.join().unwrap();

            assert!(
                frames.len() >= 2,
                "a backlog over the cap must be split into multiple frames, got {}",
                frames.len()
            );
            for frame in &frames {
                assert!(
                    frame.len() <= OUTPUT_COALESCE_CAP + CHUNK,
                    "frame of {} bytes exceeds the cap by more than one message",
                    frame.len()
                );
            }
            assert_eq!(frames.concat(), expected, "no bytes lost or reordered");
        }

        /// A non-`Output` message queued between `Output`s goes out in its
        /// original position: it ends the coalescing run, and the `Output`s on
        /// either side of it must not merge across it. Guards the `carried`
        /// handoff — dropping or reordering the interrupting message would tell
        /// the client (say) the shell exited around the wrong bytes.
        #[test]
        fn spawn_writer_does_not_coalesce_outputs_across_a_non_output_message() {
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            tx.send(DaemonMsg::Output(b"before".to_vec())).unwrap();
            tx.send(DaemonMsg::Exited { code: Some(0) }).unwrap();
            tx.send(DaemonMsg::Output(b"after".to_vec())).unwrap();
            drop(tx);

            let (mut client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(rx, server, Arc::new(crate::daemon::pane::OutputGate::new()));

            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Output(b"before".to_vec()),
                "the first Output must not absorb bytes from beyond the Exited"
            );
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Exited { code: Some(0) },
                "the interrupting message keeps its place in the sequence"
            );
            assert_eq!(
                DaemonMsg::read(&mut client).unwrap(),
                DaemonMsg::Output(b"after".to_vec())
            );
            assert!(DaemonMsg::read(&mut client).is_err());
            writer.join().unwrap();
        }

        /// A dead client (socket write fails) ends the writer thread even while
        /// the pane-side sender is still alive — otherwise every disconnect
        /// would leave a writer parked in `recv()` until the pane detached it,
        /// and `run_stream`'s join of the writer would inherit that wait.
        #[test]
        fn spawn_writer_exits_on_write_failure_while_sender_is_alive() {
            let (tx, rx) = mpsc::channel::<DaemonMsg>();
            let (client, server) = UnixStream::pair().unwrap();
            let writer = spawn_writer(rx, server, Arc::new(crate::daemon::pane::OutputGate::new()));

            // Kill the client end first, then hand the writer a message: the
            // encode hits a broken pipe and the thread must bail on its own.
            drop(client);
            tx.send(DaemonMsg::Output(b"into the void".to_vec()))
                .unwrap();

            // Bounded poll rather than a bare `join()`: the sender stays alive
            // for the whole wait, so only the write-failure path can finish the
            // thread — and a regression fails in ~5 s instead of hanging.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !writer.is_finished() && std::time::Instant::now() < deadline {
                thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(
                writer.is_finished(),
                "writer must exit once the socket write fails, without waiting for channel close"
            );
            writer.join().unwrap();
            drop(tx);
        }
    }
}

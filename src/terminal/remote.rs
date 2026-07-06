//! Client-side `RemoteTerminal`: the GUI half of the persistent-daemon design.
//!
//! It owns **nothing but a socket and a local mirror**. The PTY + child live in
//! the daemon (`daemon::pane`); we hold one Unix-domain-socket connection to it
//! (one connection == one pane) and a local `alacritty_terminal::Term` that we
//! feed from the bytes the daemon replays. The render path is the usual one (an
//! `ansi::Processor` advancing a `Term`); only the *source* of those bytes is a
//! "daemon socket" rather than a "PTY master fd".
//!
//! `RemoteTerminal` exposes the fields the view reads directly (`term`, `events`,
//! `palette`, `exited`) and the methods it calls (`write`, `resize`,
//! `foreground_cwd`, `at_prompt`, `size`), so the view treats it like any local
//! terminal.
//!
//! Threading model: a dedicated reader thread blocking-reads
//! framed [`DaemonMsg`]s and advances the local `Term`, while UI-thread calls
//! (`write`/`resize`) push framed [`ClientMsg`]s out the write half. Because both
//! the reader thread and the UI thread touch the connection, we `try_clone` the
//! stream into independent read/write halves and guard the write half with a
//! `Mutex`.

#![allow(dead_code)] // Phase 4: not wired into the view yet (integration is later).

use std::borrow::Cow;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi;

use crate::core::osc::OscTokenizer;
use crate::daemon::protocol::{ClientMsg, DaemonMsg, WinSize};
use crate::daemon::transport::{self, Stream};

use super::size::TermSize;

/// Bridges reader-thread events back to the GPUI side through an async channel
/// the view drains.
#[derive(Clone)]
pub struct EventProxy {
    tx: smol::channel::Sender<AlacEvent>,
    /// True while the reader thread replays an attach `Snapshot` (the daemon's
    /// byte ring). Queries parsed out of that history — DSR/CPR, OSC 10/11/12
    /// color probes, OSC 52 clipboard reads — were already answered when they
    /// ran live; answering them *again* would write the replies to a shell
    /// that never asked, which echoes them at the current prompt as if typed
    /// (a literal `11;rgb:…` after every restore). Historical OSC 52 writes
    /// would likewise clobber the user's clipboard, and historical BELs would
    /// flash on attach. Those events are dropped at the source while this is
    /// set; everything else (Title, Wakeup…) still flows.
    replaying: Arc<AtomicBool>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: AlacEvent) {
        if self.replaying.load(Ordering::Relaxed)
            && matches!(
                event,
                AlacEvent::PtyWrite(_)
                    | AlacEvent::ColorRequest(..)
                    | AlacEvent::ClipboardStore(..)
                    | AlacEvent::ClipboardLoad(..)
                    | AlacEvent::Bell
            )
        {
            return;
        }
        // try_send: an overfull channel just means the view is behind; dropping a
        // redundant Wakeup is harmless (the next one repaints the latest grid).
        let _ = self.tx.try_send(event);
    }
}

/// Shell prompt/command state cached from the daemon's `Prompt` messages. The
/// daemon does all the OSC 133 sniffing PTY-side; we just remember the last
/// reported values so `at_prompt()` can answer cheaply without any IPC.
#[derive(Default, Clone, Copy)]
struct ShellState {
    active: bool,
    at_prompt: bool,
    last_exit: Option<i32>,
}

/// A terminal whose PTY lives in the daemon. Mirrors `backend::Terminal`'s public
/// surface so the view can treat the two interchangeably.
pub struct RemoteTerminal {
    /// Local mirror emulator. Same type and feeding discipline as `Terminal`.
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub events: smol::channel::Receiver<AlacEvent>,
    pub palette: [alacritty_terminal::vte::ansi::Rgb; 256],
    /// Whether the pane's child has exited. The reader thread can't touch `&mut
    /// self`, so the *authoritative* flag lives in `exited_flag` (an
    /// `Arc<AtomicBool>`); this field is a cheap field-readable copy the view can
    /// poll. `poll_exited()` syncs the flag into it. See the struct docs / the
    /// handoff note for why both exist.
    pub exited: bool,
    size: TermSize,
    /// Whether the first layout's `Resize` has been sent. Until then `size` is
    /// a pre-layout placeholder and the daemon-side PTY may disagree with it
    /// (attach no longer resizes the PTY), so the first `resize()` must go
    /// through even when the laid-out size happens to equal the placeholder.
    synced_size: bool,
    /// Write half of the pane connection. Guarded by a `Mutex` because UI-thread
    /// `write`/`resize` calls (and potentially others) all push frames out the
    /// same socket; the reader thread uses its own cloned read half.
    writer: Mutex<Stream>,
    /// Foreground cwd, last reported by the daemon via `Cwd`. Shared with the
    /// reader thread, which updates it as new reports arrive.
    cwd: Arc<Mutex<Option<PathBuf>>>,
    /// Shell prompt/command state, last reported by the daemon via `Prompt`.
    shell_state: Arc<Mutex<ShellState>>,
    /// Set true by the reader thread once the child exits or the daemon
    /// disconnects. `poll_exited()` copies this into the `exited` field.
    exited_flag: Arc<AtomicBool>,
    /// Whether zle is reading the keyboard right now, sniffed client-side from
    /// *live* OSC 133 marks: `B` (prompt end — zle takes over immediately
    /// after) arms it, any other mark disarms it, and Snapshot replays never
    /// touch it (a historical `B` says nothing about now). Gates the typeahead
    /// wipe: a `^U` written before zle reads is kernel-echoed as literal junk.
    zle_reading: Arc<AtomicBool>,
    reader_thread: Option<JoinHandle<()>>,
}

impl RemoteTerminal {
    /// Connect to the daemon, spawn a fresh pane (shell) sized to `size`, and
    /// start mirroring it. Returns the terminal plus the daemon-assigned
    /// `pane_id` (the caller persists it for later session restore / `attach`).
    pub fn spawn(
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cwd: Option<PathBuf>,
    ) -> anyhow::Result<(Self, u64)> {
        let mut stream = connect()?;
        let win = win_size(size, cell_w, cell_h);

        // Ask the daemon to create the pane, then read its assigned id back. The
        // very next frames on this connection are this pane's Snapshot + Output,
        // which the reader thread (started below) will consume.
        ClientMsg::Spawn { cwd, size: win }.encode(&mut stream)?;
        let pane_id = match DaemonMsg::read(&mut stream)? {
            DaemonMsg::Spawned { pane_id } => pane_id,
            DaemonMsg::Error(msg) => {
                return Err(anyhow::anyhow!("daemon refused Spawn: {msg}"));
            }
            other => {
                return Err(anyhow::anyhow!(
                    "unexpected daemon reply to Spawn: {other:?}"
                ));
            }
        };

        let term = Self::from_stream(stream, size)?;
        Ok((term, pane_id))
    }

    /// Connect to the daemon and re-attach to an existing pane `pane_id`, then
    /// start mirroring it. The daemon answers with a `Snapshot` (its byte ring)
    /// that the reader thread replays to rebuild the current screen + scrollback,
    /// followed by live `Output`.
    pub fn attach(size: TermSize, cell_w: u16, cell_h: u16, pane_id: u64) -> anyhow::Result<Self> {
        let mut stream = connect()?;
        let win = win_size(size, cell_w, cell_h);

        // Unlike Spawn there's no synchronous reply to wait for here: the Snapshot
        // arrives as the first framed message and is handled uniformly by the
        // reader thread (advance + Wakeup), so the screen rebuilds asynchronously.
        ClientMsg::Attach { pane_id, size: win }.encode(&mut stream)?;
        Self::from_stream(stream, size)
    }

    /// Shared tail of `spawn`/`attach`: build the local `Term`, split the socket
    /// into read/write halves, and launch the reader thread.
    pub(super) fn from_stream(stream: Stream, size: TermSize) -> anyhow::Result<Self> {
        // Two independent handles to the same connection: the reader thread owns
        // the read half, the UI thread writes through the (mutex-guarded) write
        // half. Reads and writes are independent directions, so this is safe.
        let read_half = stream.try_clone()?;
        let write_half = stream;

        let (tx, rx) = smol::channel::unbounded();
        let proxy = EventProxy {
            tx,
            replaying: Arc::new(AtomicBool::new(false)),
        };

        // Scrollback depth comes from user config (clamped in `Config::sanitize`
        // to alacritty's ceiling). Read fresh from disk here: a pane spawn/attach
        // is rare, and this runs on the daemon side too, which has no GPUI global.
        let config = Config {
            scrolling_history: crate::core::config::Config::load().scrollback_limit,
            ..Config::default()
        };
        let term = Term::new(config, &size, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let cwd: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let shell_state: Arc<Mutex<ShellState>> = Arc::new(Mutex::new(ShellState::default()));
        let exited_flag = Arc::new(AtomicBool::new(false));
        let zle_reading = Arc::new(AtomicBool::new(false));

        let reader_thread = Self::spawn_reader(
            term.clone(),
            proxy,
            read_half,
            cwd.clone(),
            shell_state.clone(),
            exited_flag.clone(),
            zle_reading.clone(),
        );

        Ok(Self {
            term,
            events: rx,
            palette: super::palette::build(),
            exited: false,
            size,
            synced_size: false,
            writer: Mutex::new(write_half),
            cwd,
            shell_state,
            exited_flag,
            zle_reading,
            reader_thread: Some(reader_thread),
        })
    }

    /// The reader thread: decodes framed `DaemonMsg`s off the socket and applies
    /// each. `Snapshot`/`Output` feed the same `ansi::Processor` → `Term` path as
    /// the in-process backend (so a multi-MB Snapshot is one `advance` call),
    /// `Cwd` / `Prompt` refresh the cached state, and `Exited`/EOF end the thread.
    /// Every grid-changing message is followed by a `Wakeup` so the view repaints.
    ///
    /// Frames are decoded resumably (`protocol::take_frame`) from reads that
    /// carry a timeout whenever a DEC 2026 synchronized update is pending: an
    /// app that opens a sync frame (BSU) and never closes it (ESU) would
    /// otherwise freeze this pane's rendering forever, since the buffered bytes
    /// only flush inside `advance`. When the deadline lapses with no ESU,
    /// `stop_sync` force-flushes — the same policy as alacritty's event loop.
    fn spawn_reader(
        term: Arc<FairMutex<Term<EventProxy>>>,
        proxy: EventProxy,
        read_half: Stream,
        cwd: Arc<Mutex<Option<PathBuf>>>,
        shell: Arc<Mutex<ShellState>>,
        exited_flag: Arc<AtomicBool>,
        zle_reading: Arc<AtomicBool>,
    ) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("tty7-remote-reader".to_string())
            .spawn(move || {
                // The client end of the visible-output path: keep it off the
                // efficiency cores (see `core::threads`).
                crate::core::threads::promote_to_user_interactive();
                let mut stream = read_half;
                // The VT parser is the same type the upstream event loop uses;
                // `Term` is its `Handler`.
                let mut processor: ansi::Processor = ansi::Processor::new();
                // Sniffs OSC 9 / OSC 777 desktop-notification sequences out of the
                // live output stream. The Zed alacritty fork's `Term` doesn't surface
                // these as events (its `Event` enum has no notification variant), and
                // we already see every output byte here, so a tiny side-channel
                // scanner is the cleanest interception point — no daemon-protocol or
                // view-channel plumbing needed. Its state persists across frames so a
                // sequence split over two `Output` reads is still recognized.
                let mut osc = OscNotifyScanner::default();
                // Sniffs OSC 133 marks out of the same live stream to track
                // whether zle is reading (see the `zle_reading` field docs).
                // Client-side on purpose: the daemon protocol stays untouched,
                // so mixed client/daemon versions keep working.
                let mut zle_tok = OscTokenizer::new(&[b"133"]);
                // Bytes read but not yet framed, plus the recorded geometry
                // waiting for its paired Snapshot: the daemon sends `Size`
                // immediately followed by the ring replay, and both must apply
                // under ONE grid lock — with two separate lock scopes, the UI
                // thread's layout `resize()` could slot in between and the
                // replay would run at the layout width, mis-wrapping history
                // (the exact defect the Size frame exists to prevent).
                let mut pending: Vec<u8> = Vec::new();
                let mut pending_size: Option<WinSize> = None;
                // Sized to the daemon writer's coalesced-frame cap so one large
                // Output frame lands in a few reads instead of dozens.
                let mut scratch = vec![0u8; 256 * 1024];

                // TTY7_TRACE=1: per-second reader-loop accounting on stderr, to
                // localize throughput stalls (socket wait vs lock wait vs parse).
                let trace = std::env::var("TTY7_TRACE").is_ok_and(|v| !v.is_empty() && v != "0");
                let mut tr_last = std::time::Instant::now();
                let mut tr_bytes: u64 = 0;
                let mut tr_reads: u32 = 0;
                let mut tr_read_t = std::time::Duration::ZERO;
                let mut tr_lock_t = std::time::Duration::ZERO;
                let mut tr_adv_t = std::time::Duration::ZERO;
                let mut tr_frames: u32 = 0;

                // Shared teardown: child exit, daemon disconnect, or a protocol
                // desync all end the pane the same way.
                let teardown = || {
                    term.lock().exit();
                    exited_flag.store(true, Ordering::SeqCst);
                    proxy.send_event(AlacEvent::Wakeup);
                    proxy.send_event(AlacEvent::Exit);
                };

                // Consecutive `Output` frames coalesce here and apply as ONE
                // parser pass: one term-lock, one advance, one Wakeup per
                // burst instead of per frame. The daemon's writer merges
                // queued frames too, but a fast socket drains its channel
                // before runs build up, so at full throughput frames arrive
                // 1-2 PTY reads small and per-frame costs dominate this
                // thread. Latency-free: the batch flushes as soon as no
                // complete frame is left in `pending` — it never waits for
                // bytes that haven't arrived.
                let mut out_batch: Vec<u8> = Vec::new();

                'main: loop {
                    // Apply a batched run of Output bytes (if any): parser under
                    // the terminal lock, scanners outside it, one view wakeup.
                    // A macro so call sites stay one line without threading a
                    // dozen &muts through a helper fn.
                    macro_rules! flush_batch {
                        () => {
                            if !out_batch.is_empty() {
                                {
                                    let t0 = trace.then(std::time::Instant::now);
                                    let mut term = term.lock();
                                    let t1 = trace.then(std::time::Instant::now);
                                    processor.advance(&mut *term, &out_batch);
                                    if let (Some(t0), Some(t1)) = (t0, t1) {
                                        tr_lock_t += t1 - t0;
                                        tr_adv_t += t1.elapsed();
                                    }
                                }
                                // Scan outside the terminal lock (the scanners are
                                // independent of the grid), then post notifications.
                                let mut notes = Vec::new();
                                osc.feed(&out_batch, &mut notes);
                                for (title, body) in notes {
                                    notify_desktop(title.as_deref(), &body);
                                }
                                // Live 133 marks: `B` = prompt fully printed, zle
                                // takes the keyboard right after; anything else
                                // (C command start, D precmd, A prompt start)
                                // means it isn't reading.
                                zle_tok.feed(&out_batch, |payload| {
                                    if let Some(mark) = payload.strip_prefix(b"133;") {
                                        zle_reading.store(
                                            mark.first() == Some(&b'B'),
                                            Ordering::Relaxed,
                                        );
                                    }
                                });
                                proxy.send_event(AlacEvent::Wakeup);
                                out_batch.clear();
                            }
                        };
                    }

                    // 1) Apply every complete frame already buffered.
                    loop {
                        let frame = match crate::daemon::protocol::take_frame(&mut pending) {
                            Ok(Some(frame)) => frame,
                            Ok(None) => break,
                            Err(_) => {
                                teardown();
                                break 'main;
                            }
                        };
                        let msg = match DaemonMsg::from_frame(frame.0, frame.1) {
                            Ok(msg) => msg,
                            Err(_) => {
                                teardown();
                                break 'main;
                            }
                        };
                        match msg {
                            // The geometry the attach replay was recorded under,
                            // held until its Snapshot arrives (see `pending_size`).
                            DaemonMsg::Size(ws) => {
                                flush_batch!();
                                pending_size = Some(ws);
                            }
                            DaemonMsg::Snapshot(bytes) => {
                                flush_batch!();
                                // A Snapshot is a historical replay (rebuilding the
                                // screen on attach). `Term` emits its events
                                // synchronously from inside `advance`, so bracketing
                                // it with the `replaying` flag suppresses exactly the
                                // replay's query replies / clipboard / bell effects
                                // (see `EventProxy::replaying`); it fires no desktop
                                // notifications either (only live Output is scanned).
                                proxy.replaying.store(true, Ordering::Relaxed);
                                {
                                    let mut term = term.lock();
                                    // Size the grid to the recorded geometry *before*
                                    // replaying, or history wraps at the wrong column
                                    // and relative cursor motion lands on the wrong
                                    // rows. The view's first layout then resizes both
                                    // sides to the real pane size.
                                    if let Some(ws) = pending_size.take() {
                                        term.resize(TermSize::new(
                                            ws.cols as usize,
                                            ws.rows as usize,
                                        ));
                                    }
                                    processor.advance(&mut *term, &bytes);
                                    // The ring can end inside a sync frame (a BSU
                                    // whose ESU fell past the recording): flush it
                                    // now, still under the replaying flag — trapped
                                    // replay bytes flushing later would count as
                                    // *live* and re-answer historical queries, the
                                    // exact leak replay suppression exists to stop.
                                    if processor.sync_timeout().sync_timeout().is_some() {
                                        processor.stop_sync(&mut *term);
                                    }
                                }
                                proxy.replaying.store(false, Ordering::Relaxed);
                                proxy.send_event(AlacEvent::Wakeup);
                            }
                            DaemonMsg::Output(bytes) => {
                                // Defer: the batch applies when this run of
                                // Output frames ends (a control frame, or no
                                // complete frame left buffered).
                                out_batch.extend_from_slice(&bytes);
                                tr_frames += 1;
                            }
                            DaemonMsg::Cwd(path) => {
                                flush_batch!();
                                if let Ok(mut guard) = cwd.lock() {
                                    *guard = Some(path);
                                }
                            }
                            DaemonMsg::Prompt {
                                active,
                                at_prompt,
                                last_exit,
                            } => {
                                flush_batch!();
                                if let Ok(mut guard) = shell.lock() {
                                    *guard = ShellState {
                                        active,
                                        at_prompt,
                                        last_exit,
                                    };
                                }
                            }
                            DaemonMsg::Exited { .. } => {
                                // Child gone: apply what it printed last, then
                                // mark the emulator exited and flip the shared
                                // flag so the next `poll_exited()` surfaces it.
                                flush_batch!();
                                teardown();
                                break 'main;
                            }
                            // Spawned/PaneList/Error aren't expected on a pane stream
                            // after the handshake; ignore them defensively rather than
                            // tearing down a live pane over a stray control frame.
                            _ => {}
                        }
                    }
                    // No complete frame left buffered: apply the batched run
                    // before blocking on the socket for more.
                    flush_batch!();

                    // 2) Refill. While a synchronized update is pending, bound the
                    //    read by its deadline; an expired deadline force-flushes.
                    let timeout = match processor.sync_timeout().sync_timeout() {
                        Some(deadline) => {
                            let left =
                                deadline.saturating_duration_since(std::time::Instant::now());
                            if left.is_zero() {
                                // No ESU within the window: flush the buffered frame
                                // (as live output — it is) and re-enter the loop.
                                let mut term = term.lock();
                                processor.stop_sync(&mut *term);
                                drop(term);
                                proxy.send_event(AlacEvent::Wakeup);
                                continue;
                            }
                            Some(left)
                        }
                        None => None,
                    };
                    // Best effort: if the timeout can't be set the read just
                    // blocks, degrading to the old flush-on-next-output behavior.
                    let _ = stream.set_read_timeout(timeout);
                    if trace && tr_last.elapsed() >= std::time::Duration::from_secs(1) {
                        eprintln!(
                            "[trace client] {:.1} MB/s | {} reads ({} B/read) {} frames | read wait {:?} lock wait {:?} advance {:?}",
                            tr_bytes as f64 / tr_last.elapsed().as_secs_f64() / 1e6,
                            tr_reads,
                            if tr_reads > 0 { tr_bytes / tr_reads as u64 } else { 0 },
                            tr_frames,
                            tr_read_t,
                            tr_lock_t,
                            tr_adv_t,
                        );
                        tr_last = std::time::Instant::now();
                        tr_bytes = 0;
                        tr_reads = 0;
                        tr_frames = 0;
                        tr_read_t = std::time::Duration::ZERO;
                        tr_lock_t = std::time::Duration::ZERO;
                        tr_adv_t = std::time::Duration::ZERO;
                    }
                    let tr0 = trace.then(std::time::Instant::now);
                    match stream.read(&mut scratch) {
                        // EOF or any I/O error == the daemon went away. Same
                        // teardown as a child exit so the view stops drawing a
                        // dead pane.
                        Ok(0) => {
                            teardown();
                            break;
                        }
                        Ok(n) => {
                            if let Some(tr0) = tr0 {
                                tr_read_t += tr0.elapsed();
                                tr_reads += 1;
                                tr_bytes += n as u64;
                            }
                            pending.extend_from_slice(&scratch[..n]);
                        }
                        // The sync deadline passed with no ESU (or a spurious
                        // early wake): loop back — the deadline re-check above
                        // flushes if it truly expired.
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => {
                            teardown();
                            break;
                        }
                    }
                }
            })
            .expect("spawn remote reader thread")
    }

    /// Sync the reader thread's shared `exited_flag` into the field the view reads
    /// directly (`self.terminal.exited`). The view currently reads `exited` as a
    /// field, and the reader thread can't touch `&mut self`, so the integration
    /// layer calls this on each event drain to keep the field current.
    pub fn poll_exited(&mut self) {
        if self.exited_flag.load(Ordering::SeqCst) {
            self.exited = true;
        }
    }

    /// Send raw bytes (keyboard input, pasted text, query replies) to the pane as
    /// a `ClientMsg::Input` frame. Mirrors `Terminal::write`'s signature exactly.
    pub fn write<B: Into<Cow<'static, [u8]>>>(&self, bytes: B) {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return;
        }
        if let Ok(mut writer) = self.writer.lock() {
            // A failed write means the daemon is gone; the reader thread will
            // observe the same disconnect and mark us exited, so swallow it here.
            let _ = ClientMsg::Input(bytes.into_owned()).encode(&mut *writer);
        }
    }

    /// Resize the local grid and tell the daemon to resize the real PTY. Mirrors
    /// `Terminal::resize`: no-op when unchanged, updates `self.size`.
    pub fn resize(&mut self, size: TermSize, cell_w: u16, cell_h: u16) {
        // Dedup repeats, but always let the *first* layout through even if it
        // matches the placeholder: attach leaves the PTY size untouched, so
        // until this frame lands the daemon may disagree with `self.size`.
        //
        // The dedup also checks the *local grid's* actual dimensions, not just
        // the last requested size: the reader thread applies the daemon's
        // recorded `Size` (the attach-replay geometry) on its own schedule, and
        // when that lands *after* the first layout's resize, deduping on the
        // remembered request alone would leave the local grid stuck at the
        // replay geometry forever while the PTY runs at the layout size.
        // Re-checking the grid lets the next layout pass self-heal.
        if self.synced_size && size == self.size {
            use alacritty_terminal::grid::Dimensions as _;
            let term = self.term.lock();
            if term.columns() == size.cols && term.screen_lines() == size.rows {
                return;
            }
        }
        self.synced_size = true;
        self.size = size;
        // Resize the local mirror first so the view reflows immediately; the
        // daemon resizes its PTY (and SIGWINCHes the child) when it gets the frame.
        self.term.lock().resize(size);

        let win = win_size(size, cell_w, cell_h);
        if let Ok(mut writer) = self.writer.lock() {
            let _ = ClientMsg::Resize(win).encode(&mut *writer);
        }
    }

    /// Foreground cwd, as last reported by the daemon (OSC 7 / proc lookup happens
    /// daemon-side). Cheap cache read — no IPC, no proc query on the client.
    pub fn foreground_cwd(&self) -> Option<PathBuf> {
        self.cwd.lock().ok().and_then(|g| g.clone())
    }

    /// Whether the shell sits idle at its prompt, from the daemon's last `Prompt`
    /// report. Only meaningful once `active` (the daemon has seen OSC 133);
    /// before that we conservatively answer `false`, matching `Terminal`'s
    /// non-macOS fallback shape.
    pub fn at_prompt(&self) -> bool {
        self.shell_state
            .lock()
            .map(|s| s.active && s.at_prompt)
            .unwrap_or(false)
    }

    /// Whether shell integration has engaged at all (the daemon has seen any
    /// OSC 133 from this pane). False for the whole rc-sourcing window after
    /// spawn, and forever for shells without integration. Gates the gap-input
    /// hold: without integration no prompt report will ever come to adopt
    /// held keys, so holding would only add latency.
    pub fn shell_active(&self) -> bool {
        self.shell_state.lock().map(|s| s.active).unwrap_or(false)
    }

    /// Whether zle is reading the keyboard right now (live `133;B` seen, no
    /// later mark). See the field docs; this is the gate for writing the
    /// typeahead wipe without it echoing into the scrollback.
    pub fn zle_reading(&self) -> bool {
        self.zle_reading.load(Ordering::Relaxed)
    }

    pub fn size(&self) -> TermSize {
        self.size
    }

    /// Query the daemon for its live panes over a short-lived control connection.
    /// Used at session restore to decide, per saved leaf, whether to `attach` to a
    /// still-running pane or `spawn` a fresh one. Returns an empty list on any
    /// error (no daemon, refused, malformed reply) so restore degrades to
    /// all-fresh.
    pub fn list_panes() -> Vec<crate::daemon::protocol::PaneInfo> {
        fn query() -> anyhow::Result<Vec<crate::daemon::protocol::PaneInfo>> {
            let mut stream = connect()?;
            ClientMsg::List.encode(&mut stream)?;
            match DaemonMsg::read(&mut stream)? {
                DaemonMsg::PaneList(list) => Ok(list),
                other => Err(anyhow::anyhow!("unexpected reply to List: {other:?}")),
            }
        }
        query().unwrap_or_default()
    }

    /// Tell the daemon to terminate a pane's child and forget it, over a
    /// short-lived control connection. Used when the user explicitly closes a tab
    /// or split pane (as opposed to quitting the app, where panes are *detached*
    /// and kept alive for restore). Best-effort: a missing daemon means there's
    /// nothing to kill anyway.
    pub fn kill_pane(pane_id: u64) {
        if let Ok(mut stream) = connect() {
            let _ = ClientMsg::Kill { pane_id }.encode(&mut stream);
            // Give the daemon a moment to read the frame before the connection
            // closes; a tiny blocking read of EOF is enough to order it.
            let _ = stream.shutdown(std::net::Shutdown::Write);
        }
    }
}

impl Drop for RemoteTerminal {
    fn drop(&mut self) {
        // Detach (don't kill): the daemon keeps the pane running so a later
        // `attach` can reconnect. Best-effort — if the socket's already dead the
        // pane is detached anyway.
        if let Ok(mut writer) = self.writer.lock() {
            let _ = ClientMsg::Detach.encode(&mut *writer);
            // Shutting the connection down unblocks the reader thread's blocking
            // read (it sees the peer close), so its `join` below returns promptly.
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Post a best-effort desktop notification via `notify-rust`. The single
/// notification entry point for the whole app: both the OSC 9 / 777 escape-sequence
/// path (the reader thread) and the "long command finished" heuristic in the view
/// route through here, so there's exactly one place that talks to the OS toast API.
///
/// `.show()` can block briefly on some platforms (a DBus round-trip on Linux, the
/// `NSUserNotification` bridge on macOS), so it runs on a detached thread — the
/// caller (the reader thread, or the UI) is never stalled, and a failure to show is
/// swallowed rather than allowed to disturb the terminal.
///
/// Note: `notify-rust`'s macOS backend uses the deprecated `NSUserNotification`,
/// which is acceptable for a completion toast.
pub(super) fn notify_desktop(title: Option<&str>, body: &str) {
    let summary = title.unwrap_or("tty7").to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        ensure_notification_app();
        let _ = notify_rust::Notification::new()
            .summary(&summary)
            .body(&body)
            .show();
    });
}

/// macOS delivers notifications *on behalf of* a registered app bundle. Pin that
/// bundle once, up front — otherwise `notify-rust` falls back to a placeholder
/// identifier (`use_default`) that Launch Services can't resolve, and macOS pops
/// a "Choose Application" file picker instead of showing the toast.
///
/// We prefer our own bundle id, which is registered once the shipped `.app` has
/// been launched; when we're an unbundled `cargo dev` binary that id isn't
/// registered (so `set_application` errors), and we fall back to Terminal's id,
/// which always exists — the notification just shows under Terminal's name.
#[cfg(target_os = "macos")]
fn ensure_notification_app() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // `com.github.tty7` matches the bundle id written by `bundle.sh`.
        if notify_rust::set_application("com.github.tty7").is_err() {
            let _ = notify_rust::set_application("com.apple.Terminal");
        }
    });
}

/// Extracts OSC 9 and OSC 777 desktop-notification sequences from a raw
/// terminal-output byte stream. The streaming OSC framing (terminators, split
/// reads, resync, payload cap) lives in `core::osc::OscTokenizer`, shared with
/// the daemon's cwd/prompt sniffer; this wrapper just names the identifiers we
/// care about and parses completed payloads into `(title, body)` notifications.
struct OscNotifyScanner {
    tok: OscTokenizer,
}

impl Default for OscNotifyScanner {
    fn default() -> Self {
        Self {
            tok: OscTokenizer::new(&[b"9", b"777"]),
        }
    }
}

impl OscNotifyScanner {
    /// Feed one chunk of output; push any recognized `(title, body)` notifications
    /// into `out` (title `None` for OSC 9, which carries only a body).
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<(Option<String>, String)>) {
        self.tok.feed(bytes, |payload| {
            if let Some(note) = parse_osc_notification(payload) {
                out.push(note);
            }
        });
    }
}

/// Parse a buffered OSC payload (the bytes after `ESC ]`, e.g. `9;Build done` or
/// `777;notify;Title;Body`) into a `(title, body)` notification, or `None` if it
/// isn't a notification we surface.
fn parse_osc_notification(payload: &[u8]) -> Option<(Option<String>, String)> {
    // OSC 9 ; <text>  — iTerm2 / growl style; title-less, body is the text.
    if let Some(rest) = payload.strip_prefix(b"9;") {
        // ConEmu overloads OSC 9 with numeric subcommands (`9;4;…` progress,
        // `9;9;<cwd>`, …); those aren't notifications, so skip a `<digit>;`/`<digit>`
        // leading field. A real message rarely starts with a bare single digit.
        let first = rest.split(|&b| b == b';').next().unwrap_or(rest);
        if first.len() == 1 && first[0].is_ascii_digit() {
            return None;
        }
        let body = String::from_utf8_lossy(rest).into_owned();
        return (!body.is_empty()).then_some((None, body));
    }
    // OSC 777 ; notify ; <title> ; <body>  — urxvt style.
    if let Some(rest) = payload.strip_prefix(b"777;notify;") {
        let mut parts = rest.splitn(2, |&b| b == b';');
        let first = String::from_utf8_lossy(parts.next().unwrap_or(b"")).into_owned();
        let second = parts
            .next()
            .map(|b| String::from_utf8_lossy(b).into_owned());
        // With both fields present it's title + body; with only one it's a body-only
        // notification (some senders omit the title).
        let (title, body) = match second {
            Some(body) if !body.is_empty() => (Some(first), body),
            _ => (None, first),
        };
        return (!body.is_empty()).then_some((title, body));
    }
    None
}

/// Open a fresh connection to the daemon's listening endpoint. The endpoint is
/// resolved through the config dir so it inherits the active `--config-dir`
/// isolation (dev vs. real config dir), exactly like every other config-dir file.
fn connect() -> anyhow::Result<Stream> {
    transport::connect().map_err(|e| {
        anyhow::anyhow!(
            "connect to daemon at {}: {e}",
            transport::endpoint_display()
        )
    })
}

/// Build the protocol `WinSize` from our `TermSize` + cell pixel size.
fn win_size(size: TermSize, cell_w: u16, cell_h: u16) -> WinSize {
    WinSize {
        cols: size.cols as u16,
        rows: size.rows as u16,
        cell_w,
        cell_h,
    }
}

// Uses `UnixStream::pair()` to stand in for the daemon connection, so it only
// runs on Unix. On Windows the transport is loopback TCP (no `pair` helper); the
// reader logic it exercises is platform-agnostic, so Unix coverage suffices.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    /// Without a real daemon, drive the reader path directly: a `UnixStream::pair`
    /// stands in for the connection. We hand `RemoteTerminal` one half (as if it
    /// were the attach'd socket) and push framed `DaemonMsg`s down the other, then
    /// assert the bytes landed in the local `Term`'s grid and the cwd was cached.
    #[test]
    fn reader_feeds_local_grid() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();

        let size = TermSize::new(80, 24);
        // Build a RemoteTerminal around the client half exactly like `from_stream`
        // does after the handshake. (We can't call `spawn`/`attach` here because
        // there's no daemon to perform the handshake.)
        let term = RemoteTerminal::from_stream(client_side, size).unwrap();

        // Send some visible output and a cwd report.
        DaemonMsg::Output(b"hello".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        DaemonMsg::Cwd(PathBuf::from("/tmp/work"))
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        // The reader thread applies frames asynchronously; poll the grid briefly
        // until "hello" shows up on row 0 (avoids a fixed-sleep flake).
        let mut got = String::new();
        for _ in 0..200 {
            {
                let t = term.term.lock();
                let grid = t.grid();
                got.clear();
                for col in 0..5usize {
                    let cell = &grid[alacritty_terminal::index::Line(0)]
                        [alacritty_terminal::index::Column(col)];
                    got.push(cell.c);
                }
            }
            if got == "hello" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(got, "hello", "reader thread should have fed the grid");

        // The `Cwd` frame is processed after `Output`, so it may land a moment
        // after "hello" shows up; poll for it rather than reading once.
        let mut cwd = None;
        for _ in 0..200 {
            cwd = term.foreground_cwd();
            if cwd.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(cwd, Some(PathBuf::from("/tmp/work")));

        // Drop the daemon side: the reader hits EOF, marks exited, and exits.
        drop(daemon_side);
        for _ in 0..200 {
            if term.exited_flag.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(term.exited_flag.load(Ordering::SeqCst));
    }

    /// Regression for the "restored pane types `11;rgb:…` at the prompt" bug:
    /// queries replayed from an attach `Snapshot` must NOT be re-answered —
    /// they were answered when they ran live, and answering again writes the
    /// reply to a shell that never asked (it echoes at the current prompt as
    /// if typed). Historical OSC 52 must not touch the clipboard and BELs must
    /// not flash either. The same sequences in *live* output keep working.
    #[test]
    fn snapshot_replay_suppresses_query_replies_and_side_effects() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // Replayed history: a cursor-position query (CSI 6n), an OSC 11
        // background probe, an OSC 52 clipboard write ("hi"), and a BEL.
        DaemonMsg::Snapshot(b"\x1b[6n\x1b]11;?\x07\x1b]52;c;aGk=\x07\x07replayed".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        // The reader sends a Wakeup after the advance; collect every event up
        // to (and past) it, then assert none of the suppressed kinds leaked.
        let mut events = Vec::new();
        for _ in 0..200 {
            while let Ok(ev) = term.events.try_recv() {
                events.push(ev);
            }
            if events.iter().any(|e| matches!(e, AlacEvent::Wakeup)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            events.iter().any(|e| matches!(e, AlacEvent::Wakeup)),
            "the replay's Wakeup should still arrive"
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AlacEvent::PtyWrite(_)
                    | AlacEvent::ColorRequest(..)
                    | AlacEvent::ClipboardStore(..)
                    | AlacEvent::ClipboardLoad(..)
                    | AlacEvent::Bell
            )),
            "replayed history must not re-answer queries or replay side effects"
        );

        // The same cursor-position query in live output is answered as usual.
        DaemonMsg::Output(b"\x1b[6n".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        let mut got_reply = false;
        for _ in 0..200 {
            while let Ok(ev) = term.events.try_recv() {
                if matches!(ev, AlacEvent::PtyWrite(_)) {
                    got_reply = true;
                }
            }
            if got_reply {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(got_reply, "live queries must still be answered");
    }

    /// TUIs (Claude Code among them) probe DECRQM `?2026` before wrapping
    /// frames in BSU/ESU synchronized updates. The probe must come back
    /// "supported" (`;2` = reset) — otherwise the app streams frames
    /// unwrapped and a mid-frame state (rows cleared but not yet rewritten)
    /// can be painted.
    #[test]
    fn decrqm_probe_reports_sync_update_supported() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Output(b"\x1b[?2026$p".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        let mut reply = None;
        for _ in 0..200 {
            while let Ok(ev) = term.events.try_recv() {
                if let AlacEvent::PtyWrite(text) = ev {
                    reply = Some(text);
                }
            }
            if reply.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            reply.as_deref(),
            Some("\x1b[?2026;2$y"),
            "DECRQM ?2026 must be answered as supported (2 = reset)"
        );
    }

    #[test]
    fn win_size_carries_grid_and_cell_dims() {
        let ws = win_size(TermSize::new(80, 24), 8, 17);
        assert_eq!(ws.cols, 80);
        assert_eq!(ws.rows, 24);
        assert_eq!(ws.cell_w, 8);
        assert_eq!(ws.cell_h, 17);
    }

    /// `write` frames non-empty input as a `ClientMsg::Input`; the empty case sends
    /// nothing so the daemon never sees a zero-byte frame.
    #[test]
    fn write_sends_input_frames() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // An empty write is a no-op (asserted first so no frame precedes the real one).
        term.write(Vec::<u8>::new());
        term.write(b"echo hi\r".to_vec());

        match ClientMsg::read(&mut daemon_side).unwrap() {
            ClientMsg::Input(bytes) => assert_eq!(bytes, b"echo hi\r"),
            other => panic!("expected Input, got {other:?}"),
        }
    }

    /// Regression for the "restored pane scribbles typed text over old prompts"
    /// bug: the daemon reports the geometry the ring was recorded under
    /// (`DaemonMsg::Size`, ahead of the `Snapshot`), and the reader must apply
    /// it *before* replaying — otherwise history wraps at the placeholder
    /// width and ZLE's relative cursor motion lands on the wrong rows.
    #[test]
    fn attach_replay_runs_at_the_daemon_reported_size() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        // 80×24 placeholder, exactly like the real pre-layout attach path.
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // The ring was recorded on a 120-column PTY: a 100-char line fits there
        // without wrapping, but would wrap at the 80-column placeholder.
        DaemonMsg::Size(WinSize {
            cols: 120,
            rows: 30,
            cell_w: 8,
            cell_h: 17,
        })
        .encode(&mut daemon_side)
        .unwrap();
        DaemonMsg::Snapshot(vec![b'x'; 100])
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        // Poll until the replay landed (column 99 of row 0 filled). Don't index
        // past column 79 until the `Size` frame has widened the grid — before
        // that the placeholder grid is only 80 columns.
        let (mut tail, mut wrapped) = (' ', ' ');
        for _ in 0..200 {
            {
                use alacritty_terminal::grid::Dimensions as _;
                let t = term.term.lock();
                let grid = t.grid();
                if grid.columns() >= 120 {
                    tail = grid[alacritty_terminal::index::Line(0)]
                        [alacritty_terminal::index::Column(99)]
                    .c;
                    wrapped = grid[alacritty_terminal::index::Line(1)]
                        [alacritty_terminal::index::Column(0)]
                    .c;
                }
            }
            if tail == 'x' {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(tail, 'x', "replay should run at the recorded 120-col width");
        assert_eq!(
            wrapped, ' ',
            "a 100-char line must not wrap on a 120-col grid"
        );
    }

    /// The first layout always syncs the daemon, even at the placeholder size:
    /// attach no longer resizes the PTY, so until the first `Resize` frame the
    /// PTY may disagree with the client grid. Only *subsequent* same-size
    /// resizes are deduplicated.
    #[test]
    fn first_resize_always_syncs_then_dedups() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // Laid out at exactly the placeholder size: the frame must still go out.
        term.resize(TermSize::new(80, 24), 8, 17);
        match ClientMsg::read(&mut daemon_side).unwrap() {
            ClientMsg::Resize(ws) => assert_eq!((ws.cols, ws.rows), (80, 24)),
            other => panic!("expected the first Resize to be sent, got {other:?}"),
        }

        // The same size again is deduplicated: the next frame on the wire is
        // the Input written afterwards, not another Resize.
        term.resize(TermSize::new(80, 24), 8, 17);
        term.write(b"marker".to_vec());
        match ClientMsg::read(&mut daemon_side).unwrap() {
            ClientMsg::Input(bytes) => assert_eq!(bytes, b"marker"),
            other => panic!("expected Input (dup resize sends nothing), got {other:?}"),
        }
    }

    /// Regression: a DEC 2026 synchronized update opened (BSU) but never closed
    /// (ESU) must not freeze the pane — after the sync deadline the buffered
    /// frame force-flushes, exactly like alacritty's event loop. Before the
    /// fix the reader blocked on the socket and the bytes stayed trapped until
    /// the next output happened to arrive.
    #[test]
    fn sync_update_without_esu_flushes_after_the_deadline() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // BSU, then visible text — and no ESU, ever.
        DaemonMsg::Output(b"\x1b[?2026habc".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        // The text must appear without any further frames: only the reader's
        // own deadline enforcement can flush it. (Bounded poll well past the
        // 150ms sync window.)
        let mut got = String::new();
        for _ in 0..600 {
            {
                let t = term.term.lock();
                let grid = t.grid();
                got.clear();
                for col in 0..3usize {
                    got.push(
                        grid[alacritty_terminal::index::Line(0)]
                            [alacritty_terminal::index::Column(col)]
                        .c,
                    );
                }
            }
            if got == "abc" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(got, "abc", "dangling BSU must flush on the sync deadline");
    }

    /// A replay ring cut mid-sync-frame (BSU recorded, its ESU past the cut)
    /// must flush as part of the replay — with query suppression still active.
    /// Trapped bytes flushing later would count as live and re-answer
    /// historical queries, the exact leak replay suppression exists to stop.
    #[test]
    fn snapshot_replay_flushes_a_dangling_sync_frame_suppressed() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // The ring ends inside a sync frame that contains a cursor query.
        DaemonMsg::Snapshot(b"\x1b[?2026h\x1b[6nhi".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        // The replayed text appears promptly (flushed with the snapshot, not
        // 150ms later as live output)…
        let mut got = String::new();
        for _ in 0..200 {
            {
                let t = term.term.lock();
                let grid = t.grid();
                got.clear();
                for col in 0..2usize {
                    got.push(
                        grid[alacritty_terminal::index::Line(0)]
                            [alacritty_terminal::index::Column(col)]
                        .c,
                    );
                }
            }
            if got == "hi" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            got, "hi",
            "the trapped replay tail must flush with the snapshot"
        );

        // …and the historical query was NOT re-answered.
        let mut events = Vec::new();
        while let Ok(ev) = term.events.try_recv() {
            events.push(ev);
        }
        assert!(
            !events.iter().any(|e| matches!(e, AlacEvent::PtyWrite(_))),
            "a query inside the replayed sync tail must stay suppressed"
        );
    }

    /// Regression for the attach-time geometry race: when the daemon's recorded
    /// `Size` (replay geometry) lands *after* the view's first layout resize,
    /// deduping on the remembered request alone froze the local grid at the
    /// replay geometry forever (every later same-size layout was swallowed
    /// while the PTY ran at the layout size). The dedup must re-check the local
    /// grid, so the next layout pass self-heals.
    #[test]
    fn layout_resize_reasserts_geometry_after_a_late_size_frame() {
        use alacritty_terminal::grid::Dimensions as _;
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // First layout: 100×40. Grid follows immediately; a Resize frame goes out.
        term.resize(TermSize::new(100, 40), 8, 17);
        assert!(matches!(
            ClientMsg::read(&mut daemon_side).unwrap(),
            ClientMsg::Resize(_)
        ));

        // The daemon's attach replay (Size + Snapshot) arrives late — after the
        // layout — and rewrites the local grid to the recorded 120×30.
        DaemonMsg::Size(WinSize {
            cols: 120,
            rows: 30,
            cell_w: 8,
            cell_h: 17,
        })
        .encode(&mut daemon_side)
        .unwrap();
        DaemonMsg::Snapshot(b"old screen".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        for _ in 0..200 {
            if term.term.lock().columns() == 120 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(term.term.lock().columns(), 120, "replay geometry applied");

        // The next layout pass reports the same 100×40 as before. The stale
        // dedup swallowed this; now it must resize the grid back and re-sync
        // the daemon.
        term.resize(TermSize::new(100, 40), 8, 17);
        assert_eq!(term.term.lock().columns(), 100);
        assert_eq!(term.term.lock().screen_lines(), 40);
        assert!(matches!(
            ClientMsg::read(&mut daemon_side).unwrap(),
            ClientMsg::Resize(ws) if ws.cols == 100 && ws.rows == 40
        ));
    }

    /// `resize` to a new geometry updates the cached size and sends a `Resize`
    /// frame; repeating the same size afterwards is a no-op.
    #[test]
    fn resize_updates_size_and_notifies_daemon() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        term.resize(TermSize::new(100, 40), 9, 18);
        assert_eq!(term.size(), TermSize::new(100, 40));
        match ClientMsg::read(&mut daemon_side).unwrap() {
            ClientMsg::Resize(ws) => {
                assert_eq!((ws.cols, ws.rows, ws.cell_w, ws.cell_h), (100, 40, 9, 18));
            }
            other => panic!("expected Resize, got {other:?}"),
        }
    }

    /// `at_prompt` requires the shell to be *active* (integration engaged): a
    /// report carrying `at_prompt: true` but `active: false` must not flip it —
    /// otherwise the line editor would engage during the rc-sourcing window.
    #[test]
    fn at_prompt_stays_false_while_shell_integration_is_inactive() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        assert!(!term.shell_active(), "no report yet → integration inactive");

        // An inactive report, then an Output marker we can poll for so we know
        // the reader has processed both frames (they're applied in order).
        DaemonMsg::Prompt {
            active: false,
            at_prompt: true,
            last_exit: None,
        }
        .encode(&mut daemon_side)
        .unwrap();
        DaemonMsg::Output(b"m".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        let mut synced = false;
        for _ in 0..200 {
            let c = term.term.lock().grid()[alacritty_terminal::index::Line(0)]
                [alacritty_terminal::index::Column(0)]
            .c;
            if c == 'm' {
                synced = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(synced, "reader should have applied both frames");
        assert!(!term.shell_active());
        assert!(!term.at_prompt(), "inactive shell must gate at_prompt off");
    }

    /// `at_prompt` reflects the daemon's last `Prompt` report, and is conservatively
    /// false until the daemon has reported an active shell.
    #[test]
    fn at_prompt_follows_daemon_prompt_reports() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // Before any report, we conservatively answer false.
        assert!(!term.at_prompt());

        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(0),
        }
        .encode(&mut daemon_side)
        .unwrap();
        daemon_side.flush().unwrap();

        let mut at = false;
        for _ in 0..200 {
            if term.at_prompt() {
                at = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(at, "at_prompt should become true after the Prompt report");
    }

    /// The typeahead wipe (^U) may only be written once zle actually reads the
    /// keyboard; the client learns that from a *live* `133;B` (prompt end) in
    /// the output stream. `133;D` (command done, but precmd hooks still running
    /// with the terminal in canonical mode) must keep the flag off — a wipe
    /// written there is kernel-echoed as a literal `^U` into the scrollback —
    /// and a historical `B` replayed from an attach Snapshot is not "zle is
    /// reading right now" either.
    #[test]
    fn zle_reading_follows_live_prompt_end_marks() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        let poll = |want: bool| {
            for _ in 0..200 {
                if term.zle_reading() == want {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };
        assert!(!term.zle_reading(), "conservative false before any mark");

        // Snapshot replay carrying a historical B, then a live D with a marker
        // cell we can wait on — both applied in order by the reader.
        DaemonMsg::Snapshot(b"\x1b]133;B\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        DaemonMsg::Output(b"\x1b]133;D;0\x07m".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        let mut synced = false;
        for _ in 0..200 {
            let c = term.term.lock().grid()[alacritty_terminal::index::Line(0)]
                [alacritty_terminal::index::Column(0)]
            .c;
            if c == 'm' {
                synced = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(synced, "reader should have applied both frames");
        assert!(
            !term.zle_reading(),
            "replayed B / live D must not arm the flag"
        );

        // The live B arms it; the next command start (C) disarms it.
        DaemonMsg::Output(b"\x1b]133;B\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(true), "live B should arm zle_reading");

        DaemonMsg::Output(b"\x1b]133;C\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(false), "C (command start) should disarm zle_reading");
    }
}

/// OSC notification scanner tests. Not `unix`-gated: the scanner is pure byte logic
/// with no socket dependency, so it exercises on every platform.
#[cfg(test)]
mod osc_tests {
    use super::{OscNotifyScanner, parse_osc_notification};

    /// Run the scanner over one or more chunks and collect the notifications.
    fn scan(chunks: &[&[u8]]) -> Vec<(Option<String>, String)> {
        let mut s = OscNotifyScanner::default();
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, &mut out);
        }
        out
    }

    #[test]
    fn osc9_bel_and_st_terminators() {
        // BEL-terminated OSC 9.
        assert_eq!(
            scan(&[b"\x1b]9;Build done\x07"]),
            vec![(None, "Build done".to_string())]
        );
        // ST-terminated (ESC \) OSC 9.
        assert_eq!(
            scan(&[b"\x1b]9;Tests passed\x1b\\"]),
            vec![(None, "Tests passed".to_string())]
        );
    }

    #[test]
    fn osc777_notify_title_and_body() {
        assert_eq!(
            scan(&[b"\x1b]777;notify;Title;Body text\x07"]),
            vec![(Some("Title".to_string()), "Body text".to_string())]
        );
        // Title-only becomes a body-only notification.
        assert_eq!(
            scan(&[b"\x1b]777;notify;Just a message\x1b\\"]),
            vec![(None, "Just a message".to_string())]
        );
    }

    #[test]
    fn split_across_reads_is_reassembled() {
        // The sequence is torn across three chunks, including mid-payload and right
        // before the terminator.
        assert_eq!(
            scan(&[b"\x1b]9;Hel", b"lo wor", b"ld\x07"]),
            vec![(None, "Hello world".to_string())]
        );
        // ESC and its ST backslash split across the chunk boundary.
        assert_eq!(
            scan(&[b"\x1b]9;Ping\x1b", b"\\"]),
            vec![(None, "Ping".to_string())]
        );
    }

    #[test]
    fn uninteresting_osc_is_ignored_cheaply() {
        // OSC 52 (clipboard) and OSC 0 (title) must not produce notifications, and
        // real output around them still works.
        assert_eq!(
            scan(&[b"\x1b]52;c;bWFueSBieXRlcw==\x07\x1b]0;my title\x07"]),
            vec![]
        );
        // A notification after an ignored OSC is still caught (state resets).
        assert_eq!(
            scan(&[b"\x1b]0;title\x07\x1b]9;After\x07"]),
            vec![(None, "After".to_string())]
        );
    }

    #[test]
    fn conemu_osc9_subcommands_are_not_notifications() {
        // ConEmu progress (9;4;…) and set-cwd (9;9;…) are control, not toasts.
        assert_eq!(scan(&[b"\x1b]9;4;1;50\x07"]), vec![]);
        assert_eq!(scan(&[b"\x1b]9;9;/home/u\x07"]), vec![]);
    }

    #[test]
    fn parse_rejects_empty_and_unrelated() {
        assert_eq!(parse_osc_notification(b"9;"), None);
        assert_eq!(parse_osc_notification(b"777;notify;"), None);
        assert_eq!(parse_osc_notification(b"8;;https://example.com"), None);
    }

    #[test]
    fn resyncs_on_new_osc_after_an_unterminated_one() {
        // An unterminated OSC aborted by the ESC that *opens the next* OSC must not
        // swallow that opening `]`: the following well-formed notification is still
        // caught. Covers both the buffering path (a 9/777-prefixed OSC) and the
        // ignore path (an OSC we skip, e.g. a title). Real senders occasionally omit
        // the terminator and rely on the next ESC to abort the sequence.
        assert_eq!(
            scan(&[b"\x1b]9;dropped\x1b]9;kept\x07"]),
            vec![(None, "kept".to_string())]
        );
        assert_eq!(
            scan(&[b"\x1b]0;title\x1b]9;After title\x07"]),
            vec![(None, "After title".to_string())]
        );
    }
}

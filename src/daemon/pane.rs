//! `DaemonPane`: the daemon-side owner of one PTY + child shell.
//!
//! This is the daemon analogue of the client's mirror terminal, but **headless**:
//! it runs no alacritty `Term` and does no rendering. The reader thread instead
//! (a) appends raw PTY bytes to a bounded *replay ring*, (b) forwards them to the
//! currently-attached client as `DaemonMsg::Output`, and (c) feeds an OSC sniffer
//! that learns the cwd (OSC 7) and prompt state (OSC 133) and pushes those to the
//! client. The client rebuilds the screen locally from a `Snapshot` (the ring
//! replayed on attach) plus the live `Output` tail.
//!
//! The PTY is driven by [`portable-pty`](portable_pty): a Unix pty on Unix and a
//! ConPTY on Windows, behind one blocking `Read`/`Write`/`resize` API. That keeps
//! this module single-path across platforms — no fd/ioctl/signal code. What stays
//! platform-specific is the foreground-process query behind the pane title / cwd
//! fallback (macOS/Linux proc APIs; a Windows process-table walk in
//! [`winproc`](crate::daemon::winproc)) and the hangup that tears the child's
//! whole process tree down.
//!
//! Shell integration (the hooks that make the shell emit OSC 7 / OSC 133) lives
//! in the sibling [`shell_integration`](crate::daemon::shell_integration) module:
//! the PTY owner is the one place that injects it, so there's a single source of
//! truth and no duplicated rc logic. It covers zsh, bash and fish; on
//! Windows (and any other shell) the pane simply launches bare and the
//! cwd/prompt sniffing stays dormant.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::core::osc::OscTokenizer;
use crate::daemon::protocol::{DaemonMsg, PaneInfo, WinSize};
use crate::daemon::shell_integration;

/// The Windows default shell. `powershell.exe` (Windows PowerShell 5.1) ships
/// with every supported Windows, so it always resolves — users who prefer
/// `pwsh` 7+ or `cmd` can set `shell` in config. This is also the name shell
/// integration keys off (see [`default_shell_name`]).
#[cfg(windows)]
const WINDOWS_DEFAULT_SHELL: &str = "powershell.exe";

/// The platform default shell command, used when the user hasn't set `shell` in
/// `config.json`. On Windows `portable-pty`'s own default is `%COMSPEC%`
/// (i.e. `cmd.exe`); we override it to PowerShell so tty7's default matches what
/// the docs promise.
#[cfg(windows)]
fn default_prog() -> CommandBuilder {
    CommandBuilder::new(WINDOWS_DEFAULT_SHELL)
}

/// On Unix, defer to `portable-pty`, which launches the user's login shell from
/// the passwd database (falling back to `/bin/sh`).
#[cfg(not(windows))]
fn default_prog() -> CommandBuilder {
    CommandBuilder::new_default_prog()
}

/// The program name used to detect which shell integration applies for the
/// *default* shell. On Unix this is the login shell `portable-pty` resolved
/// (`$SHELL` / passwd). On Windows we can't ask the builder: its `get_shell()`
/// reports `%ComSpec%` (cmd.exe) regardless of what we actually spawn, so it
/// would send integration detection chasing cmd.exe and never engage — return
/// our real default (`powershell.exe`) instead.
#[cfg(windows)]
fn default_shell_name(_cmd: &CommandBuilder) -> String {
    WINDOWS_DEFAULT_SHELL.to_string()
}

#[cfg(not(windows))]
fn default_shell_name(cmd: &CommandBuilder) -> String {
    cmd.get_shell()
}

/// Default cap on the replay ring: 8 MiB. Enough to reconstruct a deep screen +
/// scrollback for a fresh attach, while bounding daemon memory per pane. When the
/// ring is full we drop the *oldest* bytes: a terminal stream is only meaningful
/// from some recent point onward, and a client's emulator tolerates a truncated
/// prefix far better than a hole punched in the middle.
const RING_CAP: usize = 8 * 1024 * 1024;

/// Backpressure between a pane's PTY reader and its connection writer: counts
/// the `Output` bytes sitting in the (unbounded) channel, and parks the reader
/// while the backlog is at the high-water mark. Without it the daemon slurps
/// the PTY far faster than a client can parse (the ring append no longer
/// throttles reads), so a long-running flood (`yes` in a pane) would grow the
/// queue without bound. Pausing the *reader* is exactly PTY backpressure: the
/// kernel buffer fills and the child blocks on write, like a slow real tty.
pub struct OutputGate {
    /// Bytes handed to the writer channel but not yet written out. Atomic —
    /// `add` runs per PTY read (~100k/s at full drain) and `sub` per socket
    /// write, so the hot paths must not take a lock. Signed so a late
    /// decrement racing a `reset` only drifts permissive (negative) instead
    /// of underflowing.
    queued: AtomicI64,
    /// Guards no data — it exists so a `sub`/`reset` notify can't slip between
    /// a parked reader's re-check of `queued` and its condvar wait (the
    /// classic lost-wakeup race). Only touched on the slow paths: an actual
    /// park, and the wakeup that crosses back below the mark.
    park: Mutex<()>,
    drained: Condvar,
}

impl OutputGate {
    /// Max Output bytes in flight before the PTY reader pauses. Sized to
    /// swallow a big burst whole (a 10+ MB `cat`, a build log dump) so the
    /// PTY drains at device speed and the client parses in its own time —
    /// while still bounding what a nonstop flood (`yes`) can pin per pane.
    const HIGH_WATER: i64 = 16 * 1024 * 1024;
    /// Upper bound on one backpressure pause. Attach/detach reset the counter;
    /// if an accounting slip ever left it stuck high anyway, this degrades to
    /// slow-drain instead of a wedged PTY.
    const MAX_WAIT: Duration = Duration::from_secs(2);

    pub(crate) fn new() -> Self {
        Self {
            queued: AtomicI64::new(0),
            park: Mutex::new(()),
            drained: Condvar::new(),
        }
    }

    /// Record `n` Output bytes handed to the writer channel.
    fn add(&self, n: usize) {
        self.queued.fetch_add(n as i64, Ordering::Relaxed);
    }

    /// Record `n` Output bytes leaving the channel (written to the socket, or
    /// dropped with a failed one — either way they no longer occupy memory).
    pub fn sub(&self, n: usize) {
        let prev = self.queued.fetch_sub(n as i64, Ordering::Relaxed);
        // Wake the parked reader only when this decrement crosses back below
        // the mark — not on every frame written. The lock makes the notify
        // ordered against a parking reader's re-check (see `park`).
        if prev >= Self::HIGH_WATER && prev - (n as i64) < Self::HIGH_WATER {
            let _park = self.park.lock().unwrap();
            self.drained.notify_all();
        }
    }

    /// Forget all in-flight accounting: the subscriber changed and any queued
    /// frames died with the old channel.
    fn reset(&self) {
        self.queued.store(0, Ordering::Relaxed);
        let _park = self.park.lock().unwrap();
        self.drained.notify_all();
    }

    /// Park the caller (the PTY reader; it must hold no locks) while the
    /// backlog is at/over the high-water mark, up to [`Self::MAX_WAIT`].
    /// Lock-free when the backlog is below the mark — the common case, checked
    /// before every PTY read.
    fn wait_below_high_water(&self) {
        if self.queued.load(Ordering::Relaxed) < Self::HIGH_WATER {
            return;
        }
        let deadline = std::time::Instant::now() + Self::MAX_WAIT;
        let mut park = self.park.lock().unwrap();
        while self.queued.load(Ordering::Relaxed) >= Self::HIGH_WATER {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return;
            }
            let (guard, _) = self.drained.wait_timeout(park, left).unwrap();
            park = guard;
        }
    }
}

/// Shared, mutable inner state of a pane. Split from the immutable handles (the
/// PTY master, writer, child) so a single `Mutex` guards everything the reader
/// thread and the connection threads both touch.
struct PaneState {
    /// The replay ring (raw PTY bytes, oldest-first), bounded to `RING_CAP`.
    /// A `VecDeque` so evicting the oldest bytes is O(evicted): with a `Vec`,
    /// every append to a full ring memmoved the whole 8 MiB to close the front
    /// gap — at the ~1 KiB-per-read cadence macOS PTYs deliver, that memmove
    /// dominated the daemon's read loop and capped drain throughput at ~5 MB/s.
    ring: VecDeque<u8>,
    /// The currently-attached client's outbound channel, or `None` when detached.
    /// v1 is single-subscriber: a new attach replaces this, and the old
    /// connection's receiver then sees its sender dropped and ends.
    subscriber: Option<Sender<DaemonMsg>>,
    /// Monotonic generation bumped on every `attach`. A connection remembers the
    /// epoch it installed; `detach` only clears the subscriber if it still owns
    /// that epoch, so a *replaced* connection tearing down can't blank the live
    /// subscriber a newer attach just installed (e.g. session-restore reattach,
    /// where the old GUI's connection lingers while the new one takes over).
    subscriber_epoch: u64,
    /// Latest cwd sniffed from OSC 7, so a fresh attach can be told immediately.
    cwd: Option<PathBuf>,
    /// Shell prompt/command state from OSC 133.
    shell: ShellState,
    /// Last geometry the PTY was sized to (spawn size, then each `resize`).
    /// Reported to a re-attaching client as `DaemonMsg::Size` so its replay of
    /// the ring runs at the geometry the ring was recorded under.
    size: WinSize,
    /// False once the child has exited; the pane lingers so its ring stays
    /// readable by a late attach.
    alive: bool,
}

/// One live pane: the PTY handles plus the shared [`PaneState`]. Shared across
/// connection threads via `Arc`; all mutable stream state lives behind the locks.
pub struct DaemonPane {
    pub id: u64,
    /// The PTY master. Kept for the pane's lifetime to `resize` it and to query the
    /// foreground process group (macOS title / cwd fallback). Behind a `Mutex`
    /// because the trait object is `Send` but not `Sync`, and the pane is shared
    /// across connection threads via `Arc`; the lock is uncontended in practice
    /// (resize is rare, the proc query rarer).
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// The PTY's input side (keyboard input / pasted text). Behind a `Mutex`
    /// because writes can arrive from different connection threads.
    writer: Mutex<Box<dyn Write + Send>>,
    /// The child shell. Behind a `Mutex` so `kill` (and `Drop`'s reap) can take it
    /// `&mut`. `kill()` hangs the child up (SIGHUP on Unix); `Drop` then waits it.
    child: Mutex<Box<dyn Child + Send + Sync>>,
    /// Child shell pid, when the platform reports one. Used to signal the
    /// process group on Unix and as the proc-query fallback target on
    /// macOS/Linux (hence dead on Windows).
    #[cfg_attr(windows, allow(dead_code))]
    shell_pid: Option<u32>,
    /// Throwaway dir backing shell integration (zsh's `ZDOTDIR`, bash's
    /// `--rcfile`), removed on drop. `None` if bare, or if the shell (fish)
    /// needed no on-disk file at all.
    integration_dir: Option<PathBuf>,
    /// Set during teardown so the reader doesn't emit a spurious exit.
    shutting_down: Arc<AtomicBool>,
    /// Output backpressure shared by the reader thread (adds + waits) and the
    /// connection's writer thread (drains). See [`OutputGate`].
    gate: Arc<OutputGate>,
    state: Arc<Mutex<PaneState>>,
    /// The reader `JoinHandle`, taken and joined in `Drop`.
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl DaemonPane {
    /// Spawn the user's shell on a fresh PTY in `cwd`, sized to `size`, and start
    /// its reader thread. `id` is the registry id the server assigns. `on_dead`
    /// fires (from the reader thread) when the child exits while *nobody is
    /// attached* — the case where no connection's detach would ever reclaim the
    /// pane; the server uses it to drop the dead pane from its registry instead
    /// of leaking the zombie child + replay ring for the daemon's lifetime.
    pub fn spawn(
        id: u64,
        cwd: Option<PathBuf>,
        size: WinSize,
        on_dead: impl FnOnce() + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        let pty_size = pty_size(size);

        let pair = native_pty_system().openpty(pty_size)?;

        // Build the shell command. A configured `shell` wins; otherwise fall back
        // to the platform default (the login shell on Unix, PowerShell on Windows).
        let configured = crate::core::config::shell_command();
        let mut cmd = match &configured {
            Some((program, args)) => {
                let mut c = CommandBuilder::new(program);
                c.args(args);
                c
            }
            None => default_prog(),
        };
        // The program tty7 is actually about to spawn, used (rather than `$SHELL`,
        // which can disagree) to detect which shell integration applies. For a
        // configured shell this is just its program string; for the platform
        // default it's whatever `default_prog()` resolved (passwd/`$SHELL` on
        // Unix, `powershell.exe` on Windows — see `default_shell_name`).
        let resolved_program = match &configured {
            Some((program, _)) => program.clone(),
            None => default_shell_name(&cmd),
        };

        // Shell integration: inject OSC 7 / OSC 133 hooks (zsh/fish/bash/PowerShell
        // — see `daemon::shell_integration`). Best effort — `None` (an unsupported
        // shell, or a bash/PowerShell with unpreservable custom args) means we
        // launch bare. A configured shell only counts as having "custom args" to
        // preserve when it actually specifies any — an empty `args: []` (just
        // picking the program) leaves nothing for bash's `--rcfile -i` to
        // conflict with.
        let has_custom_args = configured
            .as_ref()
            .is_some_and(|(_, args)| !args.is_empty());
        let integration = shell_integration::setup(Some(&resolved_program), has_custom_args);
        if let Some(integration) = &integration {
            // Bash only takes `--rcfile` as a non-login shell, so rebuild `cmd` as
            // a plain invocation of the resolved program rather than however
            // `default_prog()` would otherwise spawn it (a login shell on Unix).
            if integration.force_non_login {
                cmd = CommandBuilder::new(&resolved_program);
            }
            cmd.args(&integration.args);
            for (k, v) in &integration.env {
                cmd.env(k, v);
            }
        }
        let integration_dir = integration.as_ref().and_then(|i| i.dir.clone());

        // Working directory for the shell: an explicit `cwd` from the client wins
        // (new tab/split inheriting the active pane's dir, or session restore).
        // Otherwise fall back to the daemon's own cwd — but skip a bare "/", which
        // is what Launch Services hands a `.app` started from Finder/Dock/`open`
        // (there's no meaningful inherited dir there). In that case default to the
        // user's home, matching Terminal.app / iTerm. Launching from a shell
        // (`cargo dev`) still inherits that shell's dir, since it isn't "/".
        let fallback = std::env::current_dir()
            .ok()
            .filter(|d| d != std::path::Path::new("/"))
            .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from));
        // A `working_directory` of Home/Custom forces a base dir, but only when the
        // client didn't pass an explicit cwd (tab-inherit / session restore still
        // win). Inherit → `forced` is `None`, so we keep the fallback as before.
        let forced = crate::core::config::working_directory_base();
        if let Some(dir) = cwd.or(forced).or(fallback) {
            cmd.cwd(dir);
        }
        // Advertise a widely-available terminfo + truecolor.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // User-configured environment variables, injected last so they can
        // override the inherited environment (but not TERM/COLORTERM above, which
        // reflect our emulator's real capabilities).
        for (k, v) in crate::core::config::extra_env() {
            if k != "TERM" && k != "COLORTERM" {
                cmd.env(k, v);
            }
        }

        let child = pair.slave.spawn_command(cmd)?;
        let shell_pid = child.process_id();

        // Drop the slave handle now: the child holds its own slave fds, and our
        // extra handle must close so the master read side reports EOF when the
        // child exits (otherwise the reader thread would never see the hangup).
        drop(pair.slave);

        // An independent, *blocking* reader handle for the reader thread; the
        // master itself stays for resize + fg-process queries, and the writer is
        // taken once for input. (This is what makes the daemon's threaded model
        // work identically on Unix and Windows.)
        let reader_handle = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let state = Arc::new(Mutex::new(PaneState {
            ring: VecDeque::new(),
            subscriber: None,
            subscriber_epoch: 0,
            cwd: None,
            shell: ShellState::default(),
            size,
            alive: true,
        }));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(OutputGate::new());

        let pane = Arc::new(Self {
            id,
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            shell_pid,
            integration_dir,
            shutting_down: shutting_down.clone(),
            gate: gate.clone(),
            state: state.clone(),
            reader: Mutex::new(None),
        });

        let reader = Self::spawn_reader(state, shutting_down, gate, reader_handle, on_dead);
        *pane.reader.lock().unwrap() = Some(reader);

        Ok(pane)
    }

    /// Reader thread: blocking-reads PTY bytes and, for each chunk, (a) appends to
    /// the ring (dropping the oldest bytes past `RING_CAP`), (b) forwards them to
    /// the subscriber as `Output`, (c) sniffs OSC 7 / OSC 133 and pushes `Cwd` /
    /// `Prompt` on change. On EOF it marks the pane not-alive and sends `Exited`,
    /// keeping the ring for a later attach — or, when nobody is attached to hear
    /// the exit, hands the death to `on_dead` (see [`DaemonPane::spawn`]).
    fn spawn_reader(
        state: Arc<Mutex<PaneState>>,
        shutting_down: Arc<AtomicBool>,
        gate: Arc<OutputGate>,
        mut reader: Box<dyn Read + Send>,
        on_dead: impl FnOnce() + Send + 'static,
    ) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("tty7-daemon-pane-reader".to_string())
            .spawn(move || {
                // Every microsecond this thread spends off `read()` stalls the
                // child's writes (macOS PTY buffers are ~1 KiB deep), so don't
                // let the scheduler park it on an efficiency core.
                crate::core::threads::promote_to_user_interactive();
                let mut sniffer = OscSniffer::new();
                let mut buf = [0u8; 65536];

                // TTY7_TRACE=1: per-second PTY-drain accounting on stderr (the
                // daemon must run in the foreground to see it), to localize
                // throughput stalls (PTY wait vs lock+dispatch).
                let trace = std::env::var("TTY7_TRACE").is_ok_and(|v| !v.is_empty() && v != "0");
                let mut tr_last = std::time::Instant::now();
                let mut tr_bytes: u64 = 0;
                let mut tr_reads: u32 = 0;
                let mut tr_read_t = std::time::Duration::ZERO;
                let mut tr_disp_t = std::time::Duration::ZERO;

                loop {
                    if trace && tr_last.elapsed() >= std::time::Duration::from_secs(1) {
                        eprintln!(
                            "[trace daemon] {:.1} MB/s | {} reads ({} B/read) | pty wait {:?} dispatch {:?}",
                            tr_bytes as f64 / tr_last.elapsed().as_secs_f64() / 1e6,
                            tr_reads,
                            if tr_reads > 0 { tr_bytes / tr_reads as u64 } else { 0 },
                            tr_read_t,
                            tr_disp_t,
                        );
                        tr_last = std::time::Instant::now();
                        tr_bytes = 0;
                        tr_reads = 0;
                        tr_read_t = std::time::Duration::ZERO;
                        tr_disp_t = std::time::Duration::ZERO;
                    }
                    // Backpressure: let the writer drain before pulling more
                    // out of the PTY (no locks are held here).
                    gate.wait_below_high_water();
                    let tr0 = trace.then(std::time::Instant::now);
                    match reader.read(&mut buf) {
                        Ok(0) => break, // EOF: child exited / was hung up.
                        Ok(n) => {
                            if let Some(tr0) = tr0 {
                                tr_read_t += tr0.elapsed();
                                tr_reads += 1;
                                tr_bytes += n as u64;
                            }
                            let bytes = &buf[..n];
                            // Sniff first (cheap, over the same bytes); collect any
                            // cwd/prompt change to emit while we hold the lock.
                            let signals = sniffer.feed(bytes);

                            let tr1 = trace.then(std::time::Instant::now);
                            let mut st = state.lock().unwrap();
                            ring_append(&mut st.ring, bytes);
                            if let Some(sub) = &st.subscriber {
                                // A send error just means the client is gone; ignore
                                // it and let the next attach install a new sender.
                                // Successful sends are counted against the gate; the
                                // connection's writer thread credits them back.
                                if sub.send(DaemonMsg::Output(bytes.to_vec())).is_ok() {
                                    gate.add(n);
                                }
                            }
                            apply_signals(&mut st, signals);
                            if let Some(tr1) = tr1 {
                                tr_disp_t += tr1.elapsed();
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break, // EIO after hangup, etc.
                    }
                }

                // Child gone: mark not-alive and notify the subscriber, unless we
                // initiated teardown (the connection is ending anyway; the killer
                // also owns the registry cleanup). With a subscriber, the ring is
                // left intact and that connection's later detach reclaims the
                // pane. With *no* subscriber there is no such connection: fire
                // `on_dead` so the owner can reclaim, or the dead pane (zombie
                // child + ring + PTY fds) outlives everything in the registry.
                let mut st = state.lock().unwrap();
                st.alive = false;
                if !shutting_down.load(Ordering::SeqCst) {
                    let subscribed = st.subscriber.is_some();
                    if let Some(sub) = &st.subscriber {
                        let _ = sub.send(DaemonMsg::Exited { code: None });
                    }
                    drop(st);
                    if !subscribed {
                        on_dead();
                    }
                }
            })
            .expect("spawn daemon pane reader thread")
    }

    /// Become this pane's sole subscriber (replacing any prior one): report the
    /// recorded geometry, replay the ring as a `Snapshot`, then push the
    /// currently-known `Cwd` / `Prompt` so the fresh client is immediately in
    /// sync.
    ///
    /// The PTY is deliberately *not* resized here. A re-attaching client only
    /// knows a pre-layout placeholder size at this point; resizing to it would
    /// SIGWINCH the shell into redrawing its prompt at a bogus width — and
    /// those redraw bytes land in the ring, corrupting every later replay. The
    /// client instead sizes its grid from our `Size` frame for the replay, and
    /// sends a real `Resize` once it is laid out.
    pub fn attach(&self, subscriber: Sender<DaemonMsg>) -> u64 {
        let mut st = self.state.lock().unwrap();
        let epoch = attach_subscriber(&mut st, subscriber);
        // Frames queued to the *previous* subscriber died with its channel:
        // start this connection's accounting from zero so stale backlog can't
        // park the PTY reader against bytes nobody will ever drain. Ordered
        // with the reader's `add` by the state lock both run under.
        self.gate.reset();
        epoch
    }

    /// Clear the current subscriber (the pane keeps running), but only if `epoch`
    /// still names the current subscriber — a connection that was already replaced
    /// by a newer attach must not blank its successor. Idempotent.
    ///
    /// Returns `true` when, *after* detaching, the pane is reclaimable: the child
    /// has already exited (`!alive`) and no subscriber remains. The caller can then
    /// drop it from the registry instead of leaking it — a dead pane is never
    /// re-attached (clients spawn fresh for `!alive` panes), so removal is invisible.
    /// Computed under the one state lock with the detach, so a concurrent re-attach
    /// can't slip a subscriber in between the clear and the check.
    pub fn detach(&self, epoch: u64) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.subscriber_epoch == epoch {
            st.subscriber = None;
            // Whatever was still queued dies with the channel; clear its
            // accounting so the reader isn't left throttled against it.
            self.gate.reset();
        }
        !st.alive && st.subscriber.is_none()
    }

    /// The pane's Output backpressure gate, shared with the connection's writer
    /// thread (which credits bytes back as it drains them to the socket).
    pub fn gate(&self) -> Arc<OutputGate> {
        self.gate.clone()
    }

    /// Write raw bytes to the PTY (keyboard input / pasted text).
    pub fn write_input(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if let Ok(mut writer) = self.writer.lock() {
            // A failed write just means the child/pty is gone; the reader will see
            // the same EOF and tear the pane down, so swallow it here.
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    /// Resize the PTY so the child gets a `SIGWINCH` (Unix) / console resize
    /// (Windows). The daemon holds no grid to resize. Records the new size so a
    /// later placeholder re-attach can preserve it.
    pub fn resize(&self, size: WinSize) {
        self.state.lock().unwrap().size = size;
        // A failure just means the pty is gone, which the reader will observe as
        // EOF; `MasterPty::resize` itself takes `&self`.
        if let Ok(master) = self.master.lock() {
            let _ = master.resize(pty_size(size));
        }
    }

    /// Whether the child is still running. Part of the pane's public surface for
    /// the integration phase (session restore / pickers); `info()` also carries it.
    #[allow(dead_code)]
    pub fn alive(&self) -> bool {
        self.state.lock().unwrap().alive
    }

    /// Metadata for `List`: cwd prefers the OSC 7 report (falling back to a proc
    /// query), `title` is the foreground process basename when readable (macOS).
    pub fn info(&self) -> PaneInfo {
        let (cwd, alive) = {
            let st = self.state.lock().unwrap();
            (st.cwd.clone(), st.alive)
        };
        PaneInfo {
            pane_id: self.id,
            cwd: cwd.or_else(|| self.foreground_cwd()),
            title: self.foreground_title(),
            alive,
        }
    }

    /// Hang up the child now; the pane's `Drop` then reaps it. Used by the `Kill`
    /// control message and on registry teardown.
    pub fn kill(&self) {
        self.hangup();
    }

    /// Terminate the child and its whole process group. Signals the group with
    /// SIGHUP (graceful), lets `portable-pty` escalate on the shell pid (SIGHUP →
    /// ~200ms grace → SIGKILL), then SIGKILLs any group survivors so *every* holder
    /// of the slave PTY dies and the reader's blocking `read()` can finally EOF.
    /// Sets `shutting_down` so that EOF is treated as teardown, not a spurious exit.
    /// Idempotent — safe to call from `kill()` and again from `Drop`.
    fn hangup(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        // Graceful hangup of the whole group first (lets a shell run EXIT traps).
        #[cfg(unix)]
        self.signal_group(libc::SIGHUP);
        // Windows has no process group to signal: `portable-pty`'s `kill` below
        // terminates only the shell process, so capture and kill its descendant
        // tree *first*, while their parent links still point at the (still-live)
        // shell. Otherwise those children reparent and linger — some still attached
        // to the ConPTY, which would keep the reader's blocking read from EOFing.
        #[cfg(windows)]
        self.kill_descendants();
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
        // Force-kill anything in the group that ignored/outlived the hangup (a
        // foreground job in its own process group, a `trap '' HUP` child): without
        // this they keep the slave PTY open and the reader thread never EOFs.
        #[cfg(unix)]
        self.signal_group(libc::SIGKILL);
    }

    /// Terminate every descendant of the shell (children, grandchildren, …). The
    /// shell itself is left to `child.kill()`; this reaches the process tree the
    /// ConPTY's own teardown doesn't. Best effort — a snapshot failure or an
    /// already-exited process just means nothing to do.
    #[cfg(windows)]
    fn kill_descendants(&self) {
        if let Some(pid) = self.shell_pid {
            let procs = crate::daemon::winproc::snapshot();
            for target in crate::daemon::winproc::descendants(&procs, pid) {
                crate::daemon::winproc::terminate(target);
            }
        }
    }

    /// Post `sig` to the child's process group(s), not just the shell pid. The
    /// shell is a session/group leader (`portable-pty` `setsid`s it), so its pgid
    /// equals `shell_pid`; a job-control child (vim, less, a pager…) runs in the
    /// terminal's *foreground* process group instead, which that pgid doesn't
    /// cover — so signal both. Only the process group reaches the descendants that
    /// inherited the slave PTY; signalling the bare pid (what `child.kill()` does)
    /// leaves them holding it open and wedges the reader.
    #[cfg(unix)]
    fn signal_group(&self, sig: libc::c_int) {
        // SAFETY: `killpg` only posts a signal to a process group; a nonexistent or
        // already-dead group returns `ESRCH`, which we intentionally ignore.
        if let Some(pid) = self.shell_pid {
            unsafe {
                libc::killpg(pid as libc::pid_t, sig);
            }
        }
        let fg = self
            .master
            .lock()
            .ok()
            .and_then(|m| m.process_group_leader());
        if let Some(fg) = fg {
            if Some(fg as u32) != self.shell_pid {
                unsafe {
                    libc::killpg(fg, sig);
                }
            }
        }
    }

    /// Best-effort foreground cwd via `proc_pidinfo` (macOS), preferring the PTY's
    /// foreground process group over the shell pid.
    #[cfg(target_os = "macos")]
    fn foreground_cwd(&self) -> Option<PathBuf> {
        use std::ffi::CStr;

        let read_cwd = |pid: i32| -> Option<PathBuf> {
            if pid <= 0 {
                return None;
            }
            let mut vinfo: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
            // SAFETY: zeroed buffer of the expected type; real size passed; read
            // back only on success.
            let ret = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDVNODEPATHINFO,
                    0,
                    &mut vinfo as *mut _ as *mut libc::c_void,
                    size,
                )
            };
            if ret != size {
                return None;
            }
            // SAFETY: on success the kernel NUL-terminates `vip_path`.
            let s =
                unsafe { CStr::from_ptr(vinfo.pvi_cdir.vip_path.as_ptr() as *const libc::c_char) }
                    .to_str()
                    .ok()?;
            if s.is_empty() {
                None
            } else {
                Some(PathBuf::from(s))
            }
        };

        let pgid = self
            .master
            .lock()
            .ok()
            .and_then(|m| m.process_group_leader());
        pgid.and_then(read_cwd)
            .or_else(|| read_cwd(self.shell_pid.map(|p| p as i32).unwrap_or(0)))
    }

    /// Best-effort foreground cwd via `/proc/<pid>/cwd` (Linux), preferring the
    /// PTY's foreground process group over the shell pid.
    #[cfg(target_os = "linux")]
    fn foreground_cwd(&self) -> Option<PathBuf> {
        let read_cwd = |pid: i32| -> Option<PathBuf> {
            if pid <= 0 {
                return None;
            }
            std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
        };
        let pgid = self
            .master
            .lock()
            .ok()
            .and_then(|m| m.process_group_leader());
        pgid.and_then(read_cwd)
            .or_else(|| read_cwd(self.shell_pid.map(|p| p as i32).unwrap_or(0)))
    }

    /// Other platforms: no proc-query fallback (cwd only known via OSC 7).
    /// No cwd fallback on Windows (or other non-mac/Linux targets): reading another
    /// process's working directory needs PEB traversal via `ReadProcessMemory`,
    /// which is undocumented and brittle across bitness/elevation. cwd there comes
    /// from OSC 7 (the PowerShell shell integration emits it); `None` here just
    /// means "no out-of-band fallback", so a shell without integration reports no
    /// cwd rather than a wrong one.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn foreground_cwd(&self) -> Option<PathBuf> {
        None
    }

    /// Executable basename of the PTY's foreground process-group leader, used as the
    /// pane title (macOS/Linux).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn foreground_title(&self) -> String {
        self.master
            .lock()
            .ok()
            .and_then(|m| m.process_group_leader())
            .and_then(proc_name)
            .unwrap_or_default()
    }

    /// Windows has no pty foreground-process-group concept, so derive the title
    /// from the process table instead: the deepest command running under the shell
    /// (see [`winproc::foreground_name`](crate::daemon::winproc::foreground_name)).
    /// Empty while the shell sits idle at its prompt, which leaves the pane's
    /// existing title in place.
    #[cfg(windows)]
    fn foreground_title(&self) -> String {
        let Some(pid) = self.shell_pid else {
            return String::new();
        };
        let procs = crate::daemon::winproc::snapshot();
        crate::daemon::winproc::foreground_name(&procs, pid).unwrap_or_default()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    fn foreground_title(&self) -> String {
        String::new()
    }
}

impl Drop for DaemonPane {
    fn drop(&mut self) {
        // Hang up the child and its whole process group (SIGHUP → SIGKILL) so every
        // descendant holding the slave PTY dies and the reader's `read()` can EOF.
        self.hangup();
        // Reap the (now SIGKILLed) shell so it isn't left a zombie. It can't block
        // on a live process: SIGKILL can't be caught, so the shell is dead/dying.
        if let Ok(mut child) = self.child.lock() {
            let _ = child.wait();
        }
        // Join the reader, but *bounded*. Normally the group-kill above closed the
        // slave and the reader EOFed at once, so this returns immediately. But a
        // fully-detached descendant (its own session, still holding the slave) can
        // be beyond the reach of our signals; never let that wedge this thread —
        // `Drop` runs on a connection thread, and blocking it forever is the P0
        // hang this guards against. If the reader doesn't finish in time, leave it
        // detached (it ends on its own if the slave ever closes).
        if let Some(handle) = self.reader.lock().unwrap().take() {
            join_bounded(handle, Duration::from_secs(2));
        }
        if let Some(dir) = self.integration_dir.take() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// Join `handle`, waiting at most `timeout`. Returns `true` if the thread finished
/// (and was joined), `false` if it didn't finish in time — in which case it's left
/// running/detached. This is the backstop that keeps a stuck reader thread (blocked
/// on a `read()` that never EOFs because some descendant still holds the slave PTY)
/// from wedging the connection thread that `DaemonPane::drop` runs on. Uses a
/// throwaway joiner thread because `std::thread::JoinHandle` has no timed join.
fn join_bounded(handle: JoinHandle<()>, timeout: Duration) -> bool {
    let (tx, rx) = mpsc::channel();
    if std::thread::Builder::new()
        .name("tty7-daemon-pane-join".to_string())
        .spawn(move || {
            let _ = handle.join();
            let _ = tx.send(());
        })
        .is_err()
    {
        // Couldn't even spawn the joiner; don't block. The reader (if stuck) leaks,
        // but the connection thread is freed — the whole point.
        return false;
    }
    rx.recv_timeout(timeout).is_ok()
}

/// Map our `WinSize` (cell grid + per-cell pixel size) to `portable-pty`'s
/// `PtySize`. `pixel_width`/`pixel_height` are the *total* window pixel
/// dimensions (cols × cell_w), matching the `ws_xpixel`/`ws_ypixel` semantics the
/// PTY layer ultimately reports to the child; most programs ignore them.
fn pty_size(size: WinSize) -> PtySize {
    PtySize {
        rows: size.rows.max(1),
        cols: size.cols.max(1),
        pixel_width: size.cols.saturating_mul(size.cell_w),
        pixel_height: size.rows.saturating_mul(size.cell_h),
    }
}

/// Append `bytes` to the ring, dropping the oldest bytes if it would exceed
/// `RING_CAP`. A single write larger than the cap keeps only its trailing
/// `RING_CAP` bytes (the most recent screen state).
fn ring_append(ring: &mut VecDeque<u8>, bytes: &[u8]) {
    if bytes.len() >= RING_CAP {
        // The new chunk alone overflows the ring: keep only its tail.
        ring.clear();
        ring.extend(&bytes[bytes.len() - RING_CAP..]);
        return;
    }
    let overflow = (ring.len() + bytes.len()).saturating_sub(RING_CAP);
    if overflow > 0 {
        // Drop the oldest `overflow` bytes from the front.
        ring.drain(..overflow);
    }
    ring.extend(bytes);
}

/// The ring's bytes, oldest-first, as one contiguous `Vec` (the `Snapshot`
/// payload). One copy over the deque's two slices.
fn ring_to_vec(ring: &VecDeque<u8>) -> Vec<u8> {
    let (a, b) = ring.as_slices();
    let mut out = Vec::with_capacity(ring.len());
    out.extend_from_slice(a);
    out.extend_from_slice(b);
    out
}

/// Install `subscriber` as the pane's sole subscriber (replacing any prior
/// one) and replay the pane's known state through it. Called with the state
/// lock held (the pure core of [`DaemonPane::attach`], split out so it is
/// testable without a PTY).
///
/// Send size + snapshot + known signals *through the new channel* before we
/// install it, so the client's first frames are the replay, ahead of any live
/// `Output` the reader enqueues next. Size leads: the client must set its grid
/// geometry before replaying the ring. Installing drops the previous sender
/// (its receiver then ends — v1 single-client takeover). Returns the new
/// subscriber epoch.
fn attach_subscriber(st: &mut PaneState, subscriber: Sender<DaemonMsg>) -> u64 {
    st.subscriber_epoch += 1;

    let _ = subscriber.send(DaemonMsg::Size(st.size));
    let _ = subscriber.send(DaemonMsg::Snapshot(ring_to_vec(&st.ring)));
    if let Some(cwd) = &st.cwd {
        let _ = subscriber.send(DaemonMsg::Cwd(cwd.clone()));
    }
    if st.shell.active {
        let _ = subscriber.send(DaemonMsg::Prompt {
            active: st.shell.active,
            at_prompt: st.shell.at_prompt,
            last_exit: st.shell.last_exit_code,
        });
    }
    // A dead pane's reader thread — the one that reports the child's exit — is
    // long gone, so replay its exit too: without this an attach racing the
    // child's death (it exited between the client's `List` and its `Attach`)
    // renders the snapshot and then waits forever on a pane that will never
    // speak again, input silently swallowed.
    if !st.alive {
        let _ = subscriber.send(DaemonMsg::Exited { code: None });
    }
    st.subscriber = Some(subscriber);
    st.subscriber_epoch
}

/// Apply sniffed signals to the shared state and notify the subscriber of any cwd
/// / prompt change. Called with the state lock held.
fn apply_signals(st: &mut PaneState, signals: SniffSignals) {
    if let Some(cwd) = signals.cwd {
        if st.cwd.as_ref() != Some(&cwd) {
            if let Some(sub) = &st.subscriber {
                let _ = sub.send(DaemonMsg::Cwd(cwd.clone()));
            }
            st.cwd = Some(cwd);
        }
    }
    if let Some(shell) = signals.shell {
        st.shell = shell.clone();
        if let Some(sub) = &st.subscriber {
            let _ = sub.send(DaemonMsg::Prompt {
                active: shell.active,
                at_prompt: shell.at_prompt,
                last_exit: shell.last_exit_code,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// OSC sniffer (cwd + prompt). The byte-level OSC framing lives in
// `core::osc::OscTokenizer` (shared with the client's notification scanner);
// this layer only routes completed OSC 7 / OSC 133 payloads. Instead of
// mutating shared `Arc<Mutex<..>>` it returns the changes from each `feed`, so
// the pane decides when to notify (it already holds the state lock).
// ---------------------------------------------------------------------------

/// Shell-reported prompt/command state (OSC 133).
#[derive(Default, Clone, PartialEq, Eq)]
struct ShellState {
    active: bool,
    at_prompt: bool,
    last_exit_code: Option<i32>,
}

/// Changes a `feed` call produced, if any.
#[derive(Default)]
struct SniffSignals {
    cwd: Option<PathBuf>,
    shell: Option<ShellState>,
}

struct OscSniffer {
    tok: OscTokenizer,
    /// Running shell state, updated in place as 133 markers arrive.
    shell: ShellState,
}

impl OscSniffer {
    fn new() -> Self {
        Self {
            tok: OscTokenizer::new(&[b"7", b"133"]),
            shell: ShellState::default(),
        }
    }

    /// Feed a chunk; return any cwd / shell-state change completed within it. (If a
    /// chunk completes several markers, the last cwd / last shell state wins, which
    /// is the only state worth reporting.)
    fn feed(&mut self, bytes: &[u8]) -> SniffSignals {
        let mut signals = SniffSignals::default();
        let shell = &mut self.shell;
        self.tok.feed(bytes, |payload| {
            if let Some(path) = parse_osc7(payload) {
                signals.cwd = Some(path);
            } else if let Some(rest) = payload.strip_prefix(b"133;") {
                handle_osc133(shell, rest);
                signals.shell = Some(shell.clone());
            }
        });
        signals
    }
}

/// Fold one OSC 133 marker into the running shell state.
fn handle_osc133(shell: &mut ShellState, rest: &[u8]) {
    shell.active = true;
    // `at_prompt` means "no foreground command is running" — i.e. the shell is
    // drawing or sitting at its prompt, so tty7's local line editor should own
    // the keyboard. Only `C` (command started) clears it; `A` (prompt start),
    // `B` (input begins) and `D` (command finished) all set it.
    //
    // Crucially `D` and `A` set it *before* the prompt text is printed (the
    // byte stream is always `…[D][A][prompt text][B]`), whereas `B` sits at the
    // very end of PS1. Keying `at_prompt` off `B` alone left a window: when the
    // visible prompt text arrived in an earlier PTY chunk than the trailing
    // `B`, `at_prompt` was still false while the prompt was on screen, so keys
    // typed in that gap were routed to the PTY (echoed by the shell into the
    // grid) instead of the editor — the "un-deletable char / doubled prompt"
    // glitch. Setting it as early as `D`/`A` closes that window.
    match rest.first() {
        Some(b'A') | Some(b'B') => shell.at_prompt = true,
        Some(b'C') => shell.at_prompt = false,
        Some(b'D') => {
            shell.at_prompt = true;
            shell.last_exit_code = rest
                .strip_prefix(b"D;")
                .and_then(|c| std::str::from_utf8(c).ok())
                .and_then(|s| s.trim().parse::<i32>().ok());
        }
        _ => {}
    }
}

/// Build a `PathBuf` from raw OSC-7 path bytes. On Unix paths are arbitrary bytes,
/// so we go through `OsStr` losslessly; elsewhere (Windows) we interpret them as
/// UTF-8 (OSC 7 paths are UTF-8 in practice) and drop the URI's leading slash
/// ahead of a drive letter (see [`strip_uri_drive_slash`]).
#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    let s = String::from_utf8_lossy(bytes);
    PathBuf::from(strip_uri_drive_slash(s.as_ref()))
}

/// A `file://` URI carries an absolute path with a leading `/`, but a Windows
/// drive path must drop it to be valid: `parse_osc7` hands us `/C:/Users/foo`,
/// which has to become `C:/Users/foo`. Only strips when a drive letter (`X:`)
/// follows, leaving POSIX paths (`/home/x`) and UNC shares untouched. Compiled
/// on all platforms so it's testable off Windows; only used by the non-unix
/// `path_from_bytes` above.
#[cfg_attr(unix, allow(dead_code))]
fn strip_uri_drive_slash(path: &str) -> &str {
    let b = path.as_bytes();
    if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
        &path[1..]
    } else {
        path
    }
}

/// Parse an OSC 7 `file://HOST/PATH` (or bare absolute path) payload.
fn parse_osc7(payload: &[u8]) -> Option<PathBuf> {
    let rest = payload.strip_prefix(b"7;")?;
    let path_bytes: &[u8] = if let Some(after) = rest.strip_prefix(b"file://") {
        let idx = after.iter().position(|&c| c == b'/')?;
        &after[idx..]
    } else if rest.first() == Some(&b'/') {
        rest
    } else {
        return None;
    };
    let decoded = percent_decode(path_bytes);
    if decoded.is_empty() {
        return None;
    }
    Some(path_from_bytes(&decoded))
}

/// Decode `%XX` percent-escapes.
fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            if let (Some(h), Some(l)) = (hex_val(input[i + 1]), hex_val(input[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Executable basename of `pid` via `proc_pidpath` (macOS).
#[cfg(target_os = "macos")]
fn proc_name(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: valid, correctly-sized buffer; `proc_pidpath` writes at most
    // `buf.len()` bytes and returns the count (<=0 on failure).
    let ret =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if ret <= 0 {
        return None;
    }
    let path = std::str::from_utf8(&buf[..ret as usize]).ok()?;
    Some(path.rsplit('/').next().unwrap_or(path).to_string())
}

/// Executable basename of `pid` via `/proc/<pid>/exe`, falling back to
/// `/proc/<pid>/comm` (Linux).
#[cfg(target_os = "linux")]
fn proc_name(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    // `exe` is a symlink to the full binary path. If the binary was deleted the
    // link target reads "<path> (deleted)" — strip that so the name stays clean.
    if let Ok(path) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let name = name.strip_suffix(" (deleted)").unwrap_or(name);
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    // `exe` can be unreadable (e.g. a setuid foreground process); `comm` is
    // world-readable but kernel-truncated to 15 chars — good enough for a title.
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim();
    (!comm.is_empty()).then(|| comm.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that finishes is joined and reported done — the common teardown
    /// path (group-kill closed the slave, the reader EOFed) returns cleanly.
    #[test]
    fn join_bounded_returns_true_when_the_thread_finishes() {
        let handle = std::thread::spawn(|| {});
        assert!(join_bounded(handle, Duration::from_secs(5)));
    }

    /// A reader stuck forever (models one blocked on a `read()` that never EOFs
    /// because a detached grandchild still holds the slave PTY) does *not* wedge the
    /// caller: `join_bounded` gives up after the timeout and returns `false`. This
    /// is the P0 guarantee — `DaemonPane::drop` can never block indefinitely.
    #[test]
    fn join_bounded_times_out_on_a_stuck_thread() {
        let (unblock, blocked) = mpsc::channel::<()>();
        // Blocks until `unblock` is dropped — i.e. "forever" for the test's purposes.
        let handle = std::thread::spawn(move || {
            let _ = blocked.recv();
        });
        assert!(!join_bounded(handle, Duration::from_millis(50)));
        // Let the stuck thread finish so it doesn't linger past the test.
        drop(unblock);
    }

    /// Below the high-water mark the gate never blocks the reader.
    #[test]
    fn gate_passes_below_high_water() {
        let gate = OutputGate::new();
        gate.add((OutputGate::HIGH_WATER - 1) as usize);
        let t0 = std::time::Instant::now();
        gate.wait_below_high_water();
        assert!(
            t0.elapsed() < Duration::from_millis(100),
            "no wait expected"
        );
    }

    /// At the high-water mark the reader parks until the writer credits bytes
    /// back — the backpressure that keeps a flood's backlog bounded.
    #[test]
    fn gate_parks_at_high_water_until_drained() {
        let gate = Arc::new(OutputGate::new());
        gate.add(OutputGate::HIGH_WATER as usize);

        let drainer = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                gate.sub(OutputGate::HIGH_WATER as usize);
            })
        };
        let t0 = std::time::Instant::now();
        gate.wait_below_high_water();
        let waited = t0.elapsed();
        drainer.join().unwrap();
        assert!(waited >= Duration::from_millis(40), "must park until sub()");
        assert!(
            waited < OutputGate::MAX_WAIT,
            "the drain, not the escape timeout, must unpark"
        );
    }

    /// `reset` (attach/detach) unparks a reader throttled against frames that
    /// died with a replaced subscriber channel.
    #[test]
    fn gate_reset_unparks_a_throttled_reader() {
        let gate = Arc::new(OutputGate::new());
        gate.add(OutputGate::HIGH_WATER as usize * 2);

        let resetter = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                gate.reset();
            })
        };
        let t0 = std::time::Instant::now();
        gate.wait_below_high_water();
        resetter.join().unwrap();
        assert!(t0.elapsed() < OutputGate::MAX_WAIT);
    }

    /// A late `sub` racing a `reset` (old writer thread draining after a
    /// re-attach) drives the counter negative and must not panic or wedge.
    #[test]
    fn gate_tolerates_negative_drift() {
        let gate = OutputGate::new();
        gate.sub(1024);
        gate.add(512);
        let t0 = std::time::Instant::now();
        gate.wait_below_high_water();
        assert!(t0.elapsed() < Duration::from_millis(100));
    }

    /// The ring keeps appending verbatim while under the cap.
    #[test]
    fn ring_under_cap_keeps_all() {
        let mut ring = VecDeque::new();
        ring_append(&mut ring, b"hello ");
        ring_append(&mut ring, b"world");
        assert_eq!(ring_to_vec(&ring), b"hello world");
    }

    /// Once total exceeds the cap, the oldest bytes are dropped from the front and
    /// the ring holds exactly the most recent `RING_CAP` bytes.
    #[test]
    fn ring_over_cap_drops_oldest() {
        let mut ring = VecDeque::new();
        ring_append(&mut ring, &vec![b'a'; RING_CAP]);
        assert_eq!(ring.len(), RING_CAP);
        ring_append(&mut ring, &vec![b'b'; 100]);
        assert_eq!(ring.len(), RING_CAP);
        let flat = ring_to_vec(&ring);
        assert_eq!(&flat[..RING_CAP - 100], &vec![b'a'; RING_CAP - 100][..]);
        assert_eq!(&flat[RING_CAP - 100..], &vec![b'b'; 100][..]);
    }

    /// A single chunk larger than the cap keeps only its trailing `RING_CAP` bytes.
    #[test]
    fn ring_giant_chunk_keeps_tail() {
        let mut ring = VecDeque::new();
        ring_append(&mut ring, b"seed");
        let mut big = vec![b'x'; RING_CAP];
        big.extend_from_slice(b"TAIL");
        ring_append(&mut ring, &big);
        assert_eq!(ring.len(), RING_CAP);
        assert_eq!(&ring_to_vec(&ring)[RING_CAP - 4..], b"TAIL");
    }

    /// OSC 7 cwd is sniffed and surfaced as a `cwd` signal.
    #[test]
    fn sniff_osc7_cwd() {
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]7;file://host/Users/me/dev\x07");
        assert_eq!(sig.cwd, Some(PathBuf::from("/Users/me/dev")));
    }

    /// OSC 133 B/C/D drive the shell prompt state.
    #[test]
    fn sniff_osc133_prompt() {
        let mut s = OscSniffer::new();
        let b = s.feed(b"\x1b]133;B\x07");
        assert!(b.shell.as_ref().unwrap().active);
        assert!(b.shell.as_ref().unwrap().at_prompt);

        let c = s.feed(b"\x1b]133;C\x07");
        assert!(!c.shell.as_ref().unwrap().at_prompt);

        // D (command finished) means no command is running, so we're back at the
        // prompt: at_prompt is true again (it also carries the exit code).
        let d = s.feed(b"\x1b]133;D;130\x07");
        assert!(d.shell.as_ref().unwrap().at_prompt);
        assert_eq!(d.shell.as_ref().unwrap().last_exit_code, Some(130));
    }

    /// Regression: a well-formed OSC marker directly following an *unterminated*
    /// one must not be dropped. A bare ESC inside an OSC aborts the current
    /// sequence (VT semantics) and — when the next byte is `]` — introduces a new
    /// OSC. The scanner has to resync on that `]` rather than dropping it into
    /// Ground, or the following marker is silently lost. (The resync itself now
    /// lives in `core::osc::OscTokenizer`; this stays as a routing-level guard
    /// that cwd/prompt markers survive it end to end.)
    #[test]
    fn sniff_resyncs_on_new_osc_after_an_unterminated_one() {
        // OSC 133: an unterminated `133;A` (aborted by the bare ESC that opens the
        // next OSC) immediately followed by a well-formed `133;B`. The B marker
        // drives at_prompt and must survive — dropping it re-opens the "prompt
        // visible but keys mis-routed to the PTY" window this sniffer exists to close.
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        assert!(
            sig.shell.as_ref().map(|sh| sh.at_prompt).unwrap_or(false),
            "OSC 133;B after an unterminated 133;A was dropped (no resync on `]`)"
        );

        // OSC 7: an unterminated cwd report followed by a well-formed one — the
        // second path must win (the first is discarded, not the second).
        let mut s = OscSniffer::new();
        let sig = s.feed(b"\x1b]7;file://host/dropped\x1b]7;file://host/kept\x07");
        assert_eq!(sig.cwd, Some(PathBuf::from("/kept")));
    }

    /// Regression guard for the "un-deletable char / doubled prompt" glitch.
    ///
    /// A new prompt is emitted as `…[D][A][visible PS1 text][B]`, and only the
    /// trailing `B` used to flip `at_prompt` true. When the visible text and the
    /// trailing `B` landed in *different* PTY read chunks (long prompts with git
    /// status + color escapes make this likely), there was a window where the
    /// client had already rendered the prompt — the user sees it and starts typing
    /// — yet `at_prompt` was still false, so those keys were routed to the PTY
    /// (echoed by ZLE into the grid) instead of the local editor.
    ///
    /// The fix keys `at_prompt` off "no command running", so `D`/`A` (which precede
    /// the prompt text in the stream) already set it true. This test feeds the
    /// prompt as separate chunks and asserts `at_prompt` is true from the moment
    /// the prompt text is visible — i.e. the window is closed.
    #[test]
    fn at_prompt_covers_prompt_draw_gap_across_chunks() {
        let mut s = OscSniffer::new();

        // A command was running…
        assert!(!s.feed(b"\x1b]133;C\x07").shell.as_ref().unwrap().at_prompt);

        // …then finishes: D (in its own chunk, before any prompt text) already
        // marks us back at the prompt.
        let d = s.feed(b"\x1b]133;D;0\x07");
        assert!(
            d.shell.as_ref().unwrap().at_prompt,
            "D should mark us back at the prompt before the prompt text is drawn"
        );

        // The visible prompt text arrives in a later chunk, still WITHOUT the
        // trailing B. Because D already set at_prompt, the state stays true while
        // the prompt is on screen — so a key typed here routes to the editor, not
        // the PTY. This is the window that used to be open.
        let chunk = s.feed(
            b"\x1b]133;A\x07\x1b]7;file://host/repo/tty7\x07\r\ntty7 git:(main) \xe2\x9e\x9c ",
        );
        assert!(
            chunk.shell.as_ref().unwrap().at_prompt,
            "prompt visible but at_prompt=false — the mis-routing window is still open"
        );

        // The trailing B finally arrives and keeps it true.
        assert!(s.feed(b"\x1b]133;B\x07").shell.as_ref().unwrap().at_prompt);
    }

    /// `pty_size` never reports a zero dimension (a 0×0 window would make the
    /// child think it has no room) and derives pixel size from the cell metrics.
    #[test]
    fn pty_size_clamps_and_computes_pixels() {
        let ps = pty_size(WinSize {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        });
        assert_eq!(ps.rows, 24);
        assert_eq!(ps.cols, 80);
        assert_eq!(ps.pixel_width, 80 * 8);
        assert_eq!(ps.pixel_height, 24 * 17);

        // A degenerate 0×0 window clamps rows/cols up to 1.
        let z = pty_size(WinSize {
            cols: 0,
            rows: 0,
            cell_w: 0,
            cell_h: 0,
        });
        assert_eq!(z.rows, 1);
        assert_eq!(z.cols, 1);
        assert_eq!(z.pixel_width, 0);
        assert_eq!(z.pixel_height, 0);

        // Pixel dimensions saturate rather than overflow u16.
        let big = pty_size(WinSize {
            cols: u16::MAX,
            rows: u16::MAX,
            cell_w: u16::MAX,
            cell_h: u16::MAX,
        });
        assert_eq!(big.pixel_width, u16::MAX);
        assert_eq!(big.pixel_height, u16::MAX);
    }

    /// OSC 7 parsing accepts both `file://HOST/PATH` and a bare absolute path, and
    /// rejects anything else.
    #[test]
    fn parse_osc7_forms_and_rejections() {
        // file://HOST/PATH → the path after the host.
        assert_eq!(
            parse_osc7(b"7;file://host/Users/me/dev"),
            Some(PathBuf::from("/Users/me/dev"))
        );
        // An empty host (file:///path) still yields the absolute path.
        assert_eq!(parse_osc7(b"7;file:///etc"), Some(PathBuf::from("/etc")));
        // A bare absolute path (no file:// scheme) is taken verbatim.
        assert_eq!(parse_osc7(b"7;/var/log"), Some(PathBuf::from("/var/log")));
        // Percent-escapes in the path are decoded.
        assert_eq!(
            parse_osc7(b"7;file://host/a%20b"),
            Some(PathBuf::from("/a b"))
        );
        // Percent-encoded multibyte UTF-8 (a CJK dir name) decodes losslessly.
        assert_eq!(
            parse_osc7(b"7;file://host/%E4%B8%AD%E6%96%87"),
            Some(PathBuf::from("/中文"))
        );
        // Round-trip with the shell integration's `%` → `%25` escape: a dir
        // whose name contains a literal `%XX` survives the decode intact.
        assert_eq!(
            parse_osc7(b"7;file://host/tmp/a%2520b"),
            Some(PathBuf::from("/tmp/a%20b"))
        );
        // Missing the `7;` prefix.
        assert!(parse_osc7(b"8;file://host/x").is_none());
        // `file://` with no path slash after the host.
        assert!(parse_osc7(b"7;file://host").is_none());
        // Neither file:// nor an absolute path.
        assert!(parse_osc7(b"7;relative/path").is_none());
        // Decodes to empty → rejected.
        assert!(parse_osc7(b"7;file://host").is_none());
    }

    /// A `file://` URI path arrives with a leading slash; a Windows drive path
    /// (`/C:/…`, what PowerShell's OSC 7 reporter yields) must drop it, while
    /// POSIX and UNC paths keep theirs. This is what makes cwd-inheriting new
    /// tabs work on Windows.
    #[test]
    fn strip_uri_drive_slash_only_unwraps_drive_paths() {
        assert_eq!(strip_uri_drive_slash("/C:/Users/foo"), "C:/Users/foo");
        assert_eq!(strip_uri_drive_slash("/d:/x"), "d:/x");
        // POSIX paths keep their leading slash (no drive letter follows).
        assert_eq!(strip_uri_drive_slash("/home/me/dev"), "/home/me/dev");
        // A UNC share (`//host/share`) is left alone — the second byte is a slash.
        assert_eq!(strip_uri_drive_slash("//host/share"), "//host/share");
        // No leading slash, or too short to be a drive path: untouched.
        assert_eq!(strip_uri_drive_slash("C:/already"), "C:/already");
        assert_eq!(strip_uri_drive_slash("/"), "/");
    }

    /// `%XX` escapes decode; malformed or truncated escapes are kept literally.
    #[test]
    fn percent_decode_handles_escapes_and_garbage() {
        assert_eq!(percent_decode(b"a%20b"), b"a b");
        assert_eq!(percent_decode(b"%2F"), b"/");
        assert_eq!(percent_decode(b"%2f"), b"/"); // lowercase hex
        // Non-hex after % is left verbatim.
        assert_eq!(percent_decode(b"%GG"), b"%GG");
        // A truncated escape at the end has no two following digits → literal.
        assert_eq!(percent_decode(b"x%2"), b"x%2");
        assert_eq!(percent_decode(b"plain"), b"plain");
    }

    /// `hex_val` covers the three hex ranges and rejects everything else.
    #[test]
    fn hex_val_ranges() {
        assert_eq!(hex_val(b'0'), Some(0));
        assert_eq!(hex_val(b'9'), Some(9));
        assert_eq!(hex_val(b'a'), Some(10));
        assert_eq!(hex_val(b'f'), Some(15));
        assert_eq!(hex_val(b'A'), Some(10));
        assert_eq!(hex_val(b'F'), Some(15));
        assert!(hex_val(b'g').is_none());
        assert!(hex_val(b' ').is_none());
        assert!(hex_val(b'/').is_none());
    }

    /// OSC 133 `D` carries an optional exit code; a missing or unparseable code
    /// leaves it `None`, and a negative code parses.
    #[test]
    fn osc133_exit_code_parsing() {
        let mut s = OscSniffer::new();
        // D with no code.
        let d = s.feed(b"\x1b]133;D\x07");
        assert!(d.shell.as_ref().unwrap().at_prompt);
        assert_eq!(d.shell.as_ref().unwrap().last_exit_code, None);

        // D with a non-numeric code stays None.
        let d = s.feed(b"\x1b]133;D;oops\x07");
        assert_eq!(d.shell.as_ref().unwrap().last_exit_code, None);

        // A negative exit code parses.
        let d = s.feed(b"\x1b]133;D;-1\x07");
        assert_eq!(d.shell.as_ref().unwrap().last_exit_code, Some(-1));
    }

    /// A fresh `PaneState` for the PTY-less state-machine tests.
    fn test_state(alive: bool) -> PaneState {
        PaneState {
            ring: VecDeque::new(),
            subscriber: None,
            subscriber_epoch: 0,
            cwd: None,
            shell: ShellState::default(),
            size: WinSize {
                cols: 80,
                rows: 24,
                cell_w: 8,
                cell_h: 16,
            },
            alive,
        }
    }

    /// Attaching replays Size → Snapshot (→ Cwd) in order and installs the
    /// subscriber under a fresh epoch.
    #[test]
    fn attach_replays_state_in_order_and_installs_subscriber() {
        let mut st = test_state(true);
        st.ring = VecDeque::from(b"screen".to_vec());
        st.cwd = Some(PathBuf::from("/work"));

        let (tx, rx) = mpsc::channel();
        let epoch = attach_subscriber(&mut st, tx);
        assert_eq!(epoch, 1);
        assert!(st.subscriber.is_some());
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Size(_))));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Snapshot(b)) if b == b"screen"));
        assert!(
            matches!(rx.try_recv(), Ok(DaemonMsg::Cwd(p)) if p.as_path() == std::path::Path::new("/work"))
        );
        // A live pane replays no exit; the reader thread reports that live.
        assert!(rx.try_recv().is_err());
    }

    /// Regression: attaching to a pane whose child already exited must replay
    /// the exit too — the reader thread that would have reported it is gone, so
    /// without this the client renders the snapshot and then waits forever.
    #[test]
    fn attach_to_a_dead_pane_replays_exited() {
        let mut st = test_state(false);
        st.ring = VecDeque::from(b"final screen".to_vec());

        let (tx, rx) = mpsc::channel();
        attach_subscriber(&mut st, tx);
        // Skip the geometry + snapshot replay, then the exit must follow.
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Size(_))));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Snapshot(_))));
        assert!(matches!(
            rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
    }

    /// EOF with a subscriber attached: `Exited` goes to the subscriber and the
    /// pane is NOT handed to `on_dead` — that connection's detach reclaims it.
    #[test]
    fn reader_eof_with_subscriber_sends_exited_not_on_dead() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = mpsc::channel();
        state.lock().unwrap().subscriber = Some(sub_tx);
        let dead = Arc::new(AtomicBool::new(false));
        let dead_flag = dead.clone();

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(b"tail".to_vec())),
            move || dead_flag.store(true, Ordering::SeqCst),
        );
        handle.join().unwrap(); // the Cursor EOFs immediately after "tail"

        assert!(!state.lock().unwrap().alive);
        assert_eq!(ring_to_vec(&state.lock().unwrap().ring), b"tail");
        assert!(matches!(sub_rx.try_recv(), Ok(DaemonMsg::Output(b)) if b == b"tail"));
        assert!(matches!(
            sub_rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(
            !dead.load(Ordering::SeqCst),
            "an attached death is the detach path's to reclaim, not on_dead's"
        );
    }

    /// Regression: EOF with *nobody* attached must fire `on_dead` so the server
    /// can drop the pane — otherwise a detached pane whose shell exits leaks its
    /// zombie child and replay ring in the registry for the daemon's lifetime.
    #[test]
    fn reader_eof_without_subscriber_fires_on_dead() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (dead_tx, dead_rx) = mpsc::channel();

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(Vec::new())),
            move || dead_tx.send(()).unwrap(),
        );
        handle.join().unwrap();

        assert!(!state.lock().unwrap().alive);
        assert!(dead_rx.try_recv().is_ok(), "unattached death → on_dead");
    }

    /// During owner-initiated teardown (`shutting_down`), EOF neither notifies
    /// nor fires `on_dead` — the killer owns the registry cleanup.
    #[test]
    fn reader_eof_during_shutdown_is_silent() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let dead = Arc::new(AtomicBool::new(false));
        let dead_flag = dead.clone();

        let handle = DaemonPane::spawn_reader(
            state.clone(),
            Arc::new(AtomicBool::new(true)), // teardown already initiated
            Arc::new(OutputGate::new()),
            Box::new(std::io::Cursor::new(Vec::new())),
            move || dead_flag.store(true, Ordering::SeqCst),
        );
        handle.join().unwrap();

        assert!(!state.lock().unwrap().alive);
        assert!(!dead.load(Ordering::SeqCst));
    }

    /// `apply_signals` writes sniffed cwd/shell state into the pane state.
    #[test]
    fn apply_signals_updates_state() {
        let mut st = test_state(true);

        // A cwd signal lands in the state.
        apply_signals(
            &mut st,
            SniffSignals {
                cwd: Some(PathBuf::from("/tmp/x")),
                shell: None,
            },
        );
        assert_eq!(st.cwd, Some(PathBuf::from("/tmp/x")));

        // A shell signal updates the prompt state.
        apply_signals(
            &mut st,
            SniffSignals {
                cwd: None,
                shell: Some(ShellState {
                    active: true,
                    at_prompt: true,
                    last_exit_code: Some(0),
                }),
            },
        );
        assert!(st.shell.active && st.shell.at_prompt);
        assert_eq!(st.shell.last_exit_code, Some(0));

        // An empty signal set changes nothing.
        apply_signals(&mut st, SniffSignals::default());
        assert_eq!(st.cwd, Some(PathBuf::from("/tmp/x")));
    }
}

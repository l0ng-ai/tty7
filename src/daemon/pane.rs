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
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::core::osc::OscTokenizer;
use crate::daemon::protocol::{
    AuthResponse, DaemonMsg, NativeSshSpec, PaneInfo, RemoteContext, RemoteKind, ShellSpec, WinSize,
};
use crate::daemon::shell_integration;

/// The platform default shell command, used when the user hasn't set `shell` in
/// `config.json`. On Windows `portable-pty`'s own default is `%COMSPEC%`
/// (i.e. `cmd.exe`); we override it to PowerShell — PowerShell 7 (`pwsh`) when
/// installed, probed once by `core::shells`, else the `powershell.exe` that
/// ships with every supported Windows. Mirrors Windows Terminal's preference
/// for the modern shell.
#[cfg(windows)]
fn default_prog() -> CommandBuilder {
    CommandBuilder::new(crate::core::shells::windows_default_shell())
}

/// On Unix, start from `portable-pty`'s login-shell builder, but switch to an
/// explicit command when the GUI has already detected the shell that launched
/// tty7 and forwarded it to the detached daemon. LaunchServices may otherwise
/// give the daemon a stale config dir and stale `$SHELL`.
#[cfg(not(windows))]
fn default_prog() -> CommandBuilder {
    default_prog_with_override(detected_shell_override())
}

#[cfg(not(windows))]
fn default_prog_with_override(shell_override: Option<String>) -> CommandBuilder {
    let cmd = CommandBuilder::new_default_prog();
    let portable_shell = cmd.get_shell();
    if let Some(shell) = shell_override.filter(|shell| shell != &portable_shell) {
        return CommandBuilder::new(&shell);
    }
    cmd
}

#[cfg(not(windows))]
fn detected_shell_override() -> Option<String> {
    let path = std::env::var_os(crate::daemon::DETECTED_SHELL_ENV)?;
    usable_shell_path(path)
}

#[cfg(not(windows))]
fn usable_shell_path(path: std::ffi::OsString) -> Option<String> {
    let path = PathBuf::from(path);
    if path.as_os_str().is_empty() || !path.is_file() {
        return None;
    }
    path.into_os_string().into_string().ok()
}

/// The program name used to detect which shell integration applies for the
/// *default* shell. On Unix this is the login shell `portable-pty` resolved
/// (`$SHELL` / passwd). On Windows we can't ask the builder: its `get_shell()`
/// reports `%ComSpec%` (cmd.exe) regardless of what we actually spawn, so it
/// would send integration detection chasing cmd.exe and never engage — return
/// the same PowerShell `default_prog()` resolved instead.
#[cfg(windows)]
fn default_shell_name(_cmd: &CommandBuilder) -> String {
    crate::core::shells::windows_default_shell().to_string()
}

#[cfg(not(windows))]
fn default_shell_name(cmd: &CommandBuilder) -> String {
    cmd.get_shell()
}

/// Which shell a spawn launches, by precedence: the explicit per-spawn override
/// (the new-tab dropdown) > the configured `shell` in `config.json` > `None`,
/// meaning the platform default (`default_prog()`). Kept as a function so the
/// contract is stated (and tested) in one place.
fn choose_shell(
    spawn_override: Option<ShellSpec>,
    configured: Option<(String, Vec<String>)>,
) -> Option<(String, Vec<String>)> {
    spawn_override.map(|s| (s.program, s.args)).or(configured)
}

fn apply_shell_integration(
    cmd: &mut CommandBuilder,
    resolved_program: &str,
    integration: &shell_integration::Injection,
) {
    // `CommandBuilder::new_default_prog()` preserves the Unix login-shell argv0
    // shape, but portable-pty intentionally panics if argv is appended to that
    // sentinel builder. Integrations that need argv (fish `-C`, bash `--rcfile`,
    // PowerShell flags) must use an explicit command builder first. Env-only zsh
    // integration keeps the default login-shell path.
    if integration.force_non_login || (cmd.is_default_prog() && !integration.args.is_empty()) {
        *cmd = CommandBuilder::new(resolved_program);
    }
    cmd.args(&integration.args);
    for (k, v) in &integration.env {
        cmd.env(k, v);
    }
}

struct SpawnConfig {
    cmd: CommandBuilder,
    initial_cwd: Option<PathBuf>,
    integration_dir: Option<PathBuf>,
    remote: Option<RemoteContext>,
}

fn build_spawn_config(
    cwd: Option<PathBuf>,
    shell: Option<ShellSpec>,
) -> anyhow::Result<SpawnConfig> {
    let initial_cwd = initial_working_directory(cwd);
    let (cmd, integration_dir) = build_shell_command(shell, &initial_cwd)?;
    Ok(SpawnConfig {
        cmd,
        initial_cwd,
        integration_dir,
        remote: None,
    })
}

fn build_shell_command(
    shell: Option<ShellSpec>,
    initial_cwd: &Option<PathBuf>,
) -> anyhow::Result<(CommandBuilder, Option<PathBuf>)> {
    // Build the shell command; `None` means the platform default (the login
    // shell on Unix, PowerShell on Windows).
    let configured = choose_shell(shell, crate::core::config::shell_command());
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
    // configured shell this is just its program string; for the platform default
    // it's whatever `default_prog()` resolved (passwd/`$SHELL` on Unix,
    // `powershell.exe` on Windows — see `default_shell_name`).
    let resolved_program = match &configured {
        Some((program, _)) => program.clone(),
        None => default_shell_name(&cmd),
    };

    // Shell integration: inject OSC 7 / OSC 133 hooks (zsh/fish/bash/PowerShell
    // — see `daemon::shell_integration`). Best effort — `None` (an unsupported
    // shell, or a bash/PowerShell with unpreservable custom args) means we launch
    // bare. A configured shell only counts as having "custom args" to preserve
    // when it actually specifies any — an empty `args: []` (just picking the
    // program) leaves nothing for bash's `--rcfile -i` to conflict with.
    let has_custom_args = configured
        .as_ref()
        .is_some_and(|(_, args)| !args.is_empty());
    let integration = shell_integration::setup(Some(&resolved_program), has_custom_args);
    if let Some(integration) = &integration {
        apply_shell_integration(&mut cmd, &resolved_program, integration);
    }
    let integration_dir = integration.as_ref().and_then(|i| i.dir.clone());
    apply_common_command_setup(&mut cmd, initial_cwd);
    Ok((cmd, integration_dir))
}

fn initial_working_directory(cwd: Option<PathBuf>) -> Option<PathBuf> {
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
    // win). Inherit -> `forced` is `None`, so we keep the fallback as before.
    let forced = crate::core::config::working_directory_base();
    cwd.or(forced).or(fallback)
}

fn apply_common_command_setup(cmd: &mut CommandBuilder, initial_cwd: &Option<PathBuf>) {
    if let Some(dir) = initial_cwd {
        cmd.cwd(dir);
    }
    // Advertise a widely-available terminfo + truecolor.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    // User-configured environment variables, injected last so they can override
    // the inherited environment (but not TERM/COLORTERM above, which reflect our
    // emulator's real capabilities).
    for (k, v) in crate::core::config::extra_env() {
        if k != "TERM" && k != "COLORTERM" {
            cmd.env(k, v);
        }
    }
}

/// Default cap on the replay ring: 8 MiB. Enough to reconstruct a deep screen +
/// scrollback for a fresh attach, while bounding daemon memory per pane. When the
/// ring is full we drop the *oldest* bytes: a terminal stream is only meaningful
/// from some recent point onward, and a client's emulator tolerates a truncated
/// prefix far better than a hole punched in the middle.
const RING_CAP: usize = 8 * 1024 * 1024;
const REMOTE_CONTEXT_POLL_INTERVAL: Duration = Duration::from_millis(500);

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
    /// Trusted foreground remote context from the local process table.
    remote: Option<RemoteContext>,
    /// Last geometry the PTY was sized to (spawn size, then each `resize`).
    /// Reported to a re-attaching client as `DaemonMsg::Size` so its replay of
    /// the ring runs at the geometry the ring was recorded under.
    size: WinSize,
    /// False once the child has exited; the pane lingers so its ring stays
    /// readable by a late attach.
    alive: bool,
}

/// The byte source behind a pane. The PTY path (`Pty`) is byte-for-byte the
/// original local-shell backend; `NativeSsh` is a russh shell channel bridged to
/// the same blocking reader/writer contract (see [`crate::daemon::ssh::session`]).
/// The reader thread, replay ring, `OutputGate`, and OSC sniffer are identical for
/// both — only the handle-owning bits (resize, kill/hangup, foreground queries,
/// reap) differ, and dispatch on this enum.
enum PaneBackend {
    Pty(PtyBackend),
    NativeSsh(NativeSshBackend),
}

/// The local-PTY backend: the same handles `DaemonPane` has always owned.
struct PtyBackend {
    /// The PTY master. Kept for the pane's lifetime to `resize` it and to query the
    /// foreground process group (macOS title / cwd fallback, and the reader
    /// thread's remote-prompt gate — see [`foreground_command_running`]). Behind a
    /// `Mutex` because the trait object is `Send` but not `Sync`; wrapped in an
    /// `Arc` so the reader thread can hold its own handle for that gate.
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
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
}

/// The native-SSH backend: a handle to the async channel driver (for resize /
/// close) plus the resolved remote context reported to the GUI. Resize becomes a
/// `window-change`; kill/hangup closes the channel; foreground/pgid queries are
/// `None` (a remote session has no local process group — the OSC 133 gate is a
/// no-op, which is correct, per the pipeline brief §9).
struct NativeSshBackend {
    handle: Arc<crate::daemon::ssh::SshSessionHandle>,
    /// The pane's russh connection, published by the connect task once
    /// authenticated (a `Weak`, upgraded on demand). The seam WS4/WS5 reach
    /// through [`DaemonPane::ssh_connection`].
    connection: crate::daemon::ssh::SharedConnection,
}

/// One live pane: a byte-source backend plus the shared [`PaneState`]. Shared
/// across connection threads via `Arc`; all mutable stream state lives behind the
/// locks.
pub struct DaemonPane {
    pub id: u64,
    /// The byte source (local PTY or native-SSH channel).
    backend: PaneBackend,
    /// The input side (keyboard input / pasted text): the PTY writer, or the
    /// native-SSH channel writer. Behind a `Mutex` because writes can arrive from
    /// different connection threads.
    writer: Mutex<Box<dyn Write + Send>>,
    /// Set during teardown so the reader doesn't emit a spurious exit.
    shutting_down: Arc<AtomicBool>,
    /// Output backpressure shared by the reader thread (adds + waits) and the
    /// connection's writer thread (drains). See [`OutputGate`].
    gate: Arc<OutputGate>,
    state: Arc<Mutex<PaneState>>,
    /// The reader `JoinHandle`, taken and joined in `Drop`.
    reader: Mutex<Option<JoinHandle<()>>>,
    /// Auth/host-key prompt broker for native-SSH panes; `None` for PTY panes.
    /// `run_stream` routes `ClientMsg::AuthResponse` here via
    /// [`DaemonPane::deliver_auth_response`].
    broker: Option<Arc<crate::daemon::ssh::PromptBroker>>,
}

/// Fires a pane's "child gone" notification exactly once, whichever thread
/// notices the death first. On Unix the reader thread sees it as a PTY `read()`
/// EOF and reports here. On Windows the ConPTY output pipe does *not* EOF when
/// the shell exits on its own — it only closes on `ClosePseudoConsole`, so a
/// natural `exit` / Ctrl-D would leave the reader blocked forever and the pane
/// wedged open (see [`DaemonPane::spawn_exit_monitor`]). There a separate thread
/// waits on the child handle and reports here instead. The `reported` latch keeps
/// whichever route fires second a no-op, so a subscriber never sees two `Exited`s
/// and `on_dead` runs at most once.
struct DeathReporter {
    reported: AtomicBool,
    /// The server's reclaim hook, consumed the first time the pane dies with
    /// nobody attached. Behind a `Mutex<Option<…>>` because it's a `FnOnce`
    /// shared between the reader and (on Windows) the monitor — whichever fires
    /// first takes it.
    on_dead: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl DeathReporter {
    fn new(on_dead: impl FnOnce() + Send + 'static) -> Self {
        Self {
            reported: AtomicBool::new(false),
            on_dead: Mutex::new(Some(Box::new(on_dead))),
        }
    }

    /// Mark the pane not-alive and, unless the owner already began teardown
    /// (`shutting_down` — the killer owns cleanup then), tell the attached
    /// subscriber it exited; with nobody attached, hand the pane to `on_dead` so
    /// the server drops it instead of leaking the zombie child + replay ring.
    /// Idempotent: only the first call has any effect.
    fn report(&self, state: &Mutex<PaneState>, shutting_down: &AtomicBool) {
        if self.reported.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut st = state.lock().unwrap();
        st.alive = false;
        if shutting_down.load(Ordering::SeqCst) {
            return;
        }
        let subscribed = st.subscriber.is_some();
        if let Some(sub) = &st.subscriber {
            let _ = sub.send(DaemonMsg::Exited { code: None });
        }
        drop(st);
        // A subscriber's later detach reclaims the pane, so only an *unattached*
        // death needs `on_dead` — and it fires at most once.
        if subscribed {
            return;
        }
        if let Some(on_dead) = self.on_dead.lock().unwrap().take() {
            on_dead();
        }
    }
}

impl DaemonPane {
    /// Spawn the user's shell on a fresh PTY in `cwd`, sized to `size`, and start
    /// its reader thread. `id` is the registry id the server assigns. `shell` is
    /// an explicit per-spawn override (the new-tab dropdown) that outranks the
    /// configured default — see [`choose_shell`]. `on_dead` fires (from the
    /// reader thread, or on Windows the child-exit monitor) when the child exits
    /// while *nobody is attached* — the case where no connection's detach would
    /// ever reclaim the pane; the server uses
    /// it to drop the dead pane from its registry instead of leaking the zombie
    /// child + replay ring for the daemon's lifetime.
    pub fn spawn(
        id: u64,
        cwd: Option<PathBuf>,
        size: WinSize,
        shell: Option<ShellSpec>,
        on_dead: impl FnOnce() + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        let pty_size = pty_size(size);

        let pair = native_pty_system().openpty(pty_size)?;
        let spawn = build_spawn_config(cwd, shell)?;

        let child = pair.slave.spawn_command(spawn.cmd)?;
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
            cwd: spawn.initial_cwd,
            shell: ShellState::default(),
            remote: spawn.remote.clone(),
            size,
            alive: true,
        }));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(OutputGate::new());

        let master = Arc::new(Mutex::new(pair.master));

        let pane = Arc::new(Self {
            id,
            backend: PaneBackend::Pty(PtyBackend {
                master: master.clone(),
                child: Mutex::new(child),
                shell_pid,
                integration_dir: spawn.integration_dir,
            }),
            writer: Mutex::new(writer),
            shutting_down: shutting_down.clone(),
            gate: gate.clone(),
            state: state.clone(),
            reader: Mutex::new(None),
            broker: None,
        });

        // Both the reader's EOF and (on Windows) the child-exit monitor report a
        // death through this shared, run-once latch — see [`DeathReporter`].
        let death = Arc::new(DeathReporter::new(on_dead));

        // Windows: the ConPTY output pipe never EOFs on a *natural* child exit, so
        // the reader alone would never notice `exit` / Ctrl-D and the pane would
        // hang open. Watch the shell handle directly and report through the same
        // latch. No-op on Unix, where the reader's `read()` EOF already covers it.
        #[cfg(windows)]
        Self::spawn_exit_monitor(
            shell_pid,
            state.clone(),
            pane.shutting_down.clone(),
            death.clone(),
        );

        // The reader gates a foreground program's OSC 133 prompt marks (a remote
        // shell over ssh emitting its own) out of `at_prompt`, so tty7's local
        // line editor stays disengaged for whatever is really reading the
        // keyboard — see `foreground_command_running` / issue #26.
        let fg_master = master.clone();
        let remote_master = master.clone();
        let reader = Self::spawn_reader(
            state,
            shutting_down,
            gate,
            reader_handle,
            move || foreground_command_running(&fg_master, shell_pid),
            move || foreground_remote_context(&remote_master),
            death,
        );
        *pane.reader.lock().unwrap() = Some(reader);

        Ok(pane)
    }

    /// Spawn a native-SSH pane: a russh shell channel bridged into the *same*
    /// reader/ring/gate/sniffer pipeline as a PTY pane. Returns immediately; the
    /// connect → auth → shell sequence runs on the SSH engine's runtime and drives
    /// this pane through the bridge. Auth/host-key prompts and progress ride this
    /// pane's own connection via the prompt broker; a failed connect surfaces as a
    /// normal `Exited` (the driver drops its data sender, EOFing the reader).
    pub fn spawn_native_ssh(
        id: u64,
        size: WinSize,
        spec: Box<NativeSshSpec>,
        on_dead: impl FnOnce() + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        // The async↔blocking bridge: blocking reader/writer for the daemon threads,
        // plus the async ends the channel driver takes.
        let bridge = crate::daemon::ssh::session::make_bridge();
        let reader_handle: Box<dyn Read + Send> = Box::new(bridge.reader);
        let writer: Box<dyn Write + Send> = Box::new(bridge.writer);
        // The connect task fills this once authenticated; the pane exposes it to
        // WS4/WS5 via `ssh_connection()`.
        let connection: crate::daemon::ssh::SharedConnection = Arc::new(Mutex::new(Weak::new()));

        // The remote context the GUI reads to label this as a native-SSH pane.
        // Forwarding/SFTP reach the connection through the in-memory registry.
        let target = spec
            .display_name
            .clone()
            .unwrap_or_else(|| format!("{}@{}", spec.user, spec.host));
        let remote = RemoteContext {
            kind: RemoteKind::NativeSsh,
            argv: Vec::new(),
            target,
        };

        let state = Arc::new(Mutex::new(PaneState {
            ring: VecDeque::new(),
            subscriber: None,
            subscriber_epoch: 0,
            // The remote cwd is unknown until the remote shell's OSC 7 arrives.
            cwd: None,
            shell: ShellState::default(),
            remote: Some(remote),
            size,
            alive: true,
        }));
        let shutting_down = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(OutputGate::new());

        // The prompt broker emits `AuthPrompt`/`SshStatus` frames to whatever
        // client is currently subscribed to this pane, and returns whether a
        // subscriber was present (so a prompt can wait for the attach to land).
        let broker = {
            let state = state.clone();
            crate::daemon::ssh::PromptBroker::new(Box::new(move |msg: DaemonMsg| {
                match &state.lock().unwrap().subscriber {
                    Some(sub) => sub.send(msg).is_ok(),
                    None => false,
                }
            }))
        };

        let pane = Arc::new(Self {
            id,
            backend: PaneBackend::NativeSsh(NativeSshBackend {
                handle: bridge.handle,
                connection: connection.clone(),
            }),
            writer: Mutex::new(writer),
            shutting_down: shutting_down.clone(),
            gate: gate.clone(),
            state: state.clone(),
            reader: Mutex::new(None),
            broker: Some(broker.clone()),
        });

        let death = Arc::new(DeathReporter::new(on_dead));

        // A remote session has no local PTY foreground process group, so both
        // gate closures answer "nothing local": OSC 133 marks from the remote
        // shell are trusted verbatim (correct — the remote shell *is* the session),
        // and no process-table SSH detection runs (this pane already *is* SSH).
        let reader = Self::spawn_reader(
            state,
            shutting_down,
            gate,
            reader_handle,
            || false,
            || None,
            death,
        );
        *pane.reader.lock().unwrap() = Some(reader);

        // Kick off the connection on the SSH engine's runtime.
        crate::daemon::ssh::SshManager::global().spawn_native_session(
            id,
            spec,
            size,
            broker,
            bridge.data_tx,
            bridge.cmd_rx,
            connection,
        );

        Ok(pane)
    }

    /// Deliver a GUI `AuthResponse` to a native-SSH pane's pending auth prompt.
    /// A no-op for PTY panes (no broker).
    pub fn deliver_auth_response(&self, request_id: u64, response: AuthResponse) {
        if let Some(broker) = &self.broker {
            broker.deliver(request_id, response);
        }
    }

    /// The shared russh connection behind a native-SSH pane, if any — the seam WS4
    /// (port-forwards) and WS5 (SFTP) use to open further channels on the pane's
    /// existing connection (`open_direct_tcpip` / `open_session_channel`). Returns
    /// `None` for a PTY pane, or for a native pane that hasn't finished
    /// authenticating (or whose connection has since dropped). Upgraded from a
    /// `Weak`, so holding the returned `Arc` keeps the connection alive only for as
    /// long as the caller needs it.
    #[allow(dead_code)] // seam consumed by WS4 (forwards) / WS5 (SFTP)
    pub fn ssh_connection(&self) -> Option<Arc<crate::daemon::ssh::SshConnection>> {
        match &self.backend {
            PaneBackend::NativeSsh(b) => b.connection.lock().unwrap().upgrade(),
            PaneBackend::Pty(_) => None,
        }
    }

    /// Reader thread: blocking-reads PTY bytes and, for each chunk, (a) appends to
    /// the ring (dropping the oldest bytes past `RING_CAP`), (b) forwards them to
    /// the subscriber as `Output`, (c) sniffs OSC 7 / OSC 133 and pushes `Cwd` /
    /// `Prompt` on change. On EOF it reports the death through `death` — marking
    /// the pane not-alive and sending `Exited`, keeping the ring for a later
    /// attach, or handing an unattached pane to `on_dead` (see [`DeathReporter`]).
    fn spawn_reader(
        state: Arc<Mutex<PaneState>>,
        shutting_down: Arc<AtomicBool>,
        gate: Arc<OutputGate>,
        mut reader: Box<dyn Read + Send>,
        // "Is a foreground command (not the shell) currently on the PTY?" Consulted
        // when a prompt mark arrives, to reject marks a foreground program emits —
        // see the call site and [`foreground_command_running`].
        foreground_running: impl Fn() -> bool + Send + 'static,
        foreground_remote: impl Fn() -> Option<RemoteContext> + Send + 'static,
        death: Arc<DeathReporter>,
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
                let mut next_remote_check = std::time::Instant::now();

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
                            let mut signals = sniffer.feed(bytes);

                            // Reject a prompt mark emitted by a *foreground program*
                            // rather than the shell tty7 spawned. The shell only
                            // emits its OSC 133 marks while it is the PTY's own
                            // foreground group (idle at its prompt); a mark arriving
                            // while a command owns the PTY therefore comes from that
                            // command — most visibly a remote shell over ssh drawing
                            // its own prompt. Trusting it would flip `at_prompt` true
                            // and engage tty7's *local* line editor, whose completion
                            // and history are local-only and wrong for the remote
                            // session (Tab completed local paths instead of the
                            // remote's). Drop the flag so keys pass raw to whatever is
                            // really reading them. The proc query runs only when a
                            // mark actually claims the prompt — about once per prompt.
                            // See issue #26.
                            if signals.shell.as_ref().is_some_and(|s| s.at_prompt)
                                && foreground_running()
                            {
                                if let Some(s) = signals.shell.as_mut() {
                                    s.at_prompt = false;
                                }
                            }

                            // SSH-context detection is a process-table query
                            // (sysctl/procfs). Keep it out of the state lock and
                            // off the per-chunk hot path; half-second freshness is
                            // enough for link hover/click state while keeping PTY
                            // drain latency predictable.
                            let remote = if std::time::Instant::now() >= next_remote_check {
                                next_remote_check =
                                    std::time::Instant::now() + REMOTE_CONTEXT_POLL_INTERVAL;
                                // A native-SSH pane already carries its own remote
                                // context; process-table detection must not clobber
                                // it (this pane *is* SSH). Only a plain PTY pane gets
                                // foreground `ssh` detection.
                                let managed = {
                                    let st = state.lock().unwrap();
                                    st.remote
                                        .as_ref()
                                        .is_some_and(|remote| remote.kind == RemoteKind::NativeSsh)
                                };
                                (!managed).then(&foreground_remote)
                            } else {
                                None
                            };

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
                            if let Some(remote) = remote {
                                apply_remote_context(&mut st, remote);
                            }
                            if let Some(tr1) = tr1 {
                                tr_disp_t += tr1.elapsed();
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break, // EIO after hangup, etc.
                    }
                }

                // Child gone (EOF): report the death — mark not-alive and notify
                // the subscriber, or hand an unattached pane to `on_dead` — unless
                // we initiated teardown. On Windows the monitor may have already
                // reported this same death; the latch makes the second call a
                // no-op. See [`DeathReporter::report`].
                death.report(&state, &shutting_down);
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

    /// The local-PTY backend, or `None` for a native-SSH pane. PTY-only
    /// operations (resize via master, signal groups, foreground proc queries)
    /// short-circuit when this is `None`.
    fn pty(&self) -> Option<&PtyBackend> {
        match &self.backend {
            PaneBackend::Pty(p) => Some(p),
            PaneBackend::NativeSsh(_) => None,
        }
    }

    /// Resize the byte source: a PTY gets `SIGWINCH` (Unix) / console resize
    /// (Windows); a native-SSH channel gets a `window-change` request. The daemon
    /// holds no grid to resize. Records the new size so a later placeholder
    /// re-attach can preserve it.
    pub fn resize(&self, size: WinSize) {
        self.state.lock().unwrap().size = size;
        match &self.backend {
            // A failure just means the pty is gone, which the reader will observe
            // as EOF; `MasterPty::resize` itself takes `&self`.
            PaneBackend::Pty(p) => {
                if let Ok(master) = p.master.lock() {
                    let _ = master.resize(pty_size(size));
                }
            }
            PaneBackend::NativeSsh(b) => b.handle.resize(size),
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

    pub(crate) fn remote_context(&self) -> Option<RemoteContext> {
        let cached = self.state.lock().unwrap().remote.clone();
        cached.or_else(|| self.foreground_remote_context())
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
        match &self.backend {
            PaneBackend::Pty(p) => {
                // Graceful hangup of the whole group first (lets a shell run EXIT traps).
                #[cfg(unix)]
                Self::signal_group(p, libc::SIGHUP);
                // Windows has no process group to signal: `portable-pty`'s `kill`
                // below terminates only the shell process, so capture and kill its
                // descendant tree *first*, while their parent links still point at
                // the (still-live) shell. Otherwise those children reparent and
                // linger — some still attached to the ConPTY, which would keep the
                // reader's blocking read from EOFing.
                #[cfg(windows)]
                Self::kill_descendants(p);
                if let Ok(mut child) = p.child.lock() {
                    let _ = child.kill();
                }
                // Force-kill anything in the group that ignored/outlived the hangup
                // (a foreground job in its own process group, a `trap '' HUP`
                // child): without this they keep the slave PTY open and the reader
                // thread never EOFs.
                #[cfg(unix)]
                Self::signal_group(p, libc::SIGKILL);
            }
            // A native-SSH pane has no local child/pgid: closing the channel ends
            // the driver, which drops its data sender and EOFs the reader — the
            // same teardown a PTY hangup produces. `shutting_down` (set above) makes
            // that EOF a silent teardown, not a spurious `Exited`.
            PaneBackend::NativeSsh(b) => b.handle.close(),
        }
    }

    /// Terminate every descendant of the shell (children, grandchildren, …). The
    /// shell itself is left to `child.kill()`; this reaches the process tree the
    /// ConPTY's own teardown doesn't. Best effort — a snapshot failure or an
    /// already-exited process just means nothing to do.
    #[cfg(windows)]
    fn kill_descendants(pty: &PtyBackend) {
        if let Some(pid) = pty.shell_pid {
            let procs = crate::daemon::winproc::snapshot();
            for target in crate::daemon::winproc::descendants(&procs, pid) {
                crate::daemon::winproc::terminate(target);
            }
        }
    }

    /// Windows-only: watch the shell for a *natural* exit (`exit`, Ctrl-D, a
    /// crash) the ConPTY reader can't observe. The ConPTY output pipe reports EOF
    /// only once the pseudoconsole is closed (`ClosePseudoConsole`), not when the
    /// child dies — and the reader itself holds a `master` handle (the fg-gate
    /// clone), so it can keep its own pipe open. Without this a shell that exits
    /// on its own leaves the reader blocked and the pane wedged open forever.
    ///
    /// We open a wait-only handle to the shell *now*, while it's alive, so pid
    /// reuse can't retarget the wait, then block a thread on it. When it signals,
    /// the death flows through the shared latch — the same `Exited` / `on_dead`
    /// the reader's EOF drives on Unix. The kill path is unaffected: it sets
    /// `shutting_down`, under which `report` is a silent no-op.
    #[cfg(windows)]
    fn spawn_exit_monitor(
        shell_pid: Option<u32>,
        state: Arc<Mutex<PaneState>>,
        shutting_down: Arc<AtomicBool>,
        death: Arc<DeathReporter>,
    ) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };

        let Some(pid) = shell_pid else { return };
        // SAFETY: `OpenProcess` on a currently-live pid for wait-only access. A null
        // return (already gone, or access denied) is handled below; on success the
        // handle is closed by the monitor thread after its single wait.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            // Couldn't watch it — report at once rather than risk wedging the pane
            // open. Opening a just-spawned child essentially never fails.
            death.report(&state, &shutting_down);
            return;
        }
        // `HANDLE` is a raw pointer and thus `!Send`; move it across the thread
        // boundary as an integer and rebuild it inside. It names a kernel object we
        // own for the handle's lifetime, so this is just relocating ownership.
        let handle = handle as isize;
        std::thread::Builder::new()
            .name("tty7-daemon-pane-exit-monitor".to_string())
            .spawn(move || {
                let handle = handle as windows_sys::Win32::Foundation::HANDLE;
                // SAFETY: `handle` is our live process handle; waited once (any
                // return — signaled or failed — means the child is effectively
                // gone), then closed exactly once.
                unsafe {
                    WaitForSingleObject(handle, INFINITE);
                    CloseHandle(handle);
                }
                death.report(&state, &shutting_down);
            })
            .expect("spawn daemon pane exit monitor thread");
    }

    /// Post `sig` to the child's process group(s), not just the shell pid. The
    /// shell is a session/group leader (`portable-pty` `setsid`s it), so its pgid
    /// equals `shell_pid`; a job-control child (vim, less, a pager…) runs in the
    /// terminal's *foreground* process group instead, which that pgid doesn't
    /// cover — so signal both. Only the process group reaches the descendants that
    /// inherited the slave PTY; signalling the bare pid (what `child.kill()` does)
    /// leaves them holding it open and wedges the reader.
    #[cfg(unix)]
    fn signal_group(pty: &PtyBackend, sig: libc::c_int) {
        // SAFETY: `killpg` only posts a signal to a process group; a nonexistent or
        // already-dead group returns `ESRCH`, which we intentionally ignore.
        if let Some(pid) = pty.shell_pid {
            unsafe {
                libc::killpg(pid as libc::pid_t, sig);
            }
        }
        let fg = pty
            .master
            .lock()
            .ok()
            .and_then(|m| m.process_group_leader());
        if let Some(fg) = fg {
            if Some(fg as u32) != pty.shell_pid {
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

        let pty = self.pty()?;
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

        let pgid = pty
            .master
            .lock()
            .ok()
            .and_then(|m| m.process_group_leader());
        pgid.and_then(read_cwd)
            .or_else(|| read_cwd(pty.shell_pid.map(|p| p as i32).unwrap_or(0)))
    }

    /// Best-effort foreground cwd via `/proc/<pid>/cwd` (Linux), preferring the
    /// PTY's foreground process group over the shell pid.
    #[cfg(target_os = "linux")]
    fn foreground_cwd(&self) -> Option<PathBuf> {
        let pty = self.pty()?;
        let read_cwd = |pid: i32| -> Option<PathBuf> {
            if pid <= 0 {
                return None;
            }
            std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
        };
        let pgid = pty
            .master
            .lock()
            .ok()
            .and_then(|m| m.process_group_leader());
        pgid.and_then(read_cwd)
            .or_else(|| read_cwd(pty.shell_pid.map(|p| p as i32).unwrap_or(0)))
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
        let Some(pty) = self.pty() else {
            return String::new();
        };
        pty.master
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
        let Some(pty) = self.pty() else {
            return String::new();
        };
        let Some(pid) = pty.shell_pid else {
            return String::new();
        };
        let procs = crate::daemon::winproc::snapshot();
        crate::daemon::winproc::foreground_name(&procs, pid).unwrap_or_default()
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    fn foreground_title(&self) -> String {
        String::new()
    }

    /// Process-table SSH detection over the local PTY's foreground command. A
    /// native-SSH pane has no local process to inspect (it already carries its own
    /// `RemoteContext`), so this is `None` there.
    fn foreground_remote_context(&self) -> Option<RemoteContext> {
        match &self.backend {
            PaneBackend::Pty(p) => foreground_remote_context(&p.master),
            PaneBackend::NativeSsh(_) => None,
        }
    }
}

impl Drop for DaemonPane {
    fn drop(&mut self) {
        // A native-SSH pane's managed forwards (WS4) are attributed to this pane;
        // tear them down as the pane dies so listeners close and remote bindings are
        // cancelled — the FR-C2 blast radius when a shared connection drops takes
        // every pane through here. Detached, so it never blocks this connection
        // thread.
        if matches!(self.backend, PaneBackend::NativeSsh(_)) {
            crate::daemon::ssh::SshManager::global().teardown_pane_forwards(self.id);
        }
        // Hang up the byte source: SIGHUP → SIGKILL for a PTY child + its group, or
        // channel close for a native-SSH session — so the reader's `read()` can EOF.
        self.hangup();
        // Reap the (now SIGKILLed) shell so it isn't left a zombie. It can't block
        // on a live process: SIGKILL can't be caught, so the shell is dead/dying.
        // A native-SSH pane has no local child to reap.
        if let PaneBackend::Pty(p) = &self.backend {
            if let Ok(mut child) = p.child.lock() {
                let _ = child.wait();
            }
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
        if let PaneBackend::Pty(p) = &mut self.backend {
            if let Some(dir) = p.integration_dir.take() {
                let _ = std::fs::remove_dir_all(&dir);
            }
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
    if st.remote.is_some() {
        let _ = subscriber.send(DaemonMsg::RemoteContext(st.remote.clone()));
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

fn apply_remote_context(st: &mut PaneState, remote: Option<RemoteContext>) {
    if st.remote == remote {
        return;
    }
    if let Some(sub) = &st.subscriber {
        let _ = sub.send(DaemonMsg::RemoteContext(remote.clone()));
    }
    st.remote = remote;
}

/// Whether a foreground command — not the shell itself — currently owns the
/// PTY. True while e.g. `ssh`, `vim`, or a nested shell runs; false when the
/// shell sits idle at its own prompt (it is then the terminal's foreground
/// process group). Unknown/missing data answers false, so a bad reading never
/// suppresses a real local prompt.
///
/// This is the signal that keeps a foreground program's OSC 133 marks — a fish
/// session over ssh emitting its own prompt marks, most visibly — from engaging
/// tty7's local line editor, whose completion and history are local-only and
/// wrong for whatever is really reading the keyboard. See issue #26.
fn foreground_command_running(
    master: &Mutex<Box<dyn MasterPty + Send>>,
    shell_pid: Option<u32>,
) -> bool {
    is_foreground_command(pty_foreground_pgid(master), shell_pid)
}

/// The PTY's foreground process-group id (`pid_t`, i.e. `i32`), read from the
/// terminal via `tcgetpgrp`, or `None` when it can't be read.
#[cfg(unix)]
fn pty_foreground_pgid(master: &Mutex<Box<dyn MasterPty + Send>>) -> Option<i32> {
    master.lock().ok().and_then(|m| m.process_group_leader())
}

/// Windows conpty has no foreground-process-group concept — portable-pty doesn't
/// implement `process_group_leader` there — so there is nothing to gate on: we
/// answer `None`, leaving prompt marks handled exactly as before. (ssh from a
/// Windows tty7 is rare and uses a different model anyway.)
#[cfg(not(unix))]
fn pty_foreground_pgid(_master: &Mutex<Box<dyn MasterPty + Send>>) -> Option<i32> {
    None
}

/// Pure core of [`foreground_command_running`]: given the PTY's foreground
/// process group and the shell's pid, is the foreground group some *other*
/// process (a running command) rather than the shell idling at its prompt?
fn is_foreground_command(fg_pgid: Option<i32>, shell_pid: Option<u32>) -> bool {
    match (fg_pgid, shell_pid) {
        (Some(pg), Some(shell)) if pg > 0 => pg as u32 != shell,
        _ => false,
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn foreground_remote_context(master: &Mutex<Box<dyn MasterPty + Send>>) -> Option<RemoteContext> {
    let pid = master.lock().ok().and_then(|m| m.process_group_leader())?;
    let argv = crate::daemon::remote::foreground_argv(pid)?;
    crate::daemon::remote::parse_ssh_invocation(&argv).map(|inv| inv.context)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn foreground_remote_context(_master: &Mutex<Box<dyn MasterPty + Send>>) -> Option<RemoteContext> {
    None
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

    /// Spawn shell precedence: explicit override > configured > platform
    /// default (`None`). Locks the contract stated on [`choose_shell`].
    #[test]
    fn choose_shell_prefers_override_then_config_then_default() {
        let over = ShellSpec {
            program: "fish".into(),
            args: vec!["-l".into()],
        };
        let cfg = ("zsh".to_string(), vec!["-i".to_string()]);

        // Override wins even when a shell is configured.
        assert_eq!(
            choose_shell(Some(over.clone()), Some(cfg.clone())),
            Some(("fish".to_string(), vec!["-l".to_string()]))
        );
        // No override → the configured shell.
        assert_eq!(choose_shell(None, Some(cfg.clone())), Some(cfg));
        // Neither → platform default.
        assert_eq!(choose_shell(None, None), None);
    }

    #[test]
    fn arg_based_integration_rebuilds_default_shell_builder() {
        let mut cmd = CommandBuilder::new_default_prog();
        let injection = shell_integration::Injection {
            env: std::collections::HashMap::new(),
            args: vec!["-C".to_string(), "echo ready".to_string()],
            force_non_login: false,
            dir: None,
        };

        apply_shell_integration(&mut cmd, "/bin/fish", &injection);

        assert!(!cmd.is_default_prog(), "argv can now be appended safely");
        let argv: Vec<_> = cmd
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, vec!["/bin/fish", "-C", "echo ready"]);
    }

    #[test]
    fn env_only_integration_keeps_default_login_shell_builder() {
        let mut cmd = CommandBuilder::new_default_prog();
        let mut env = std::collections::HashMap::new();
        env.insert("ZDOTDIR".to_string(), "/tmp/tty7-zdotdir-test".to_string());
        let injection = shell_integration::Injection {
            env,
            args: Vec::new(),
            force_non_login: false,
            dir: None,
        };

        apply_shell_integration(&mut cmd, "/bin/zsh", &injection);

        assert!(
            cmd.is_default_prog(),
            "zsh still launches as the login shell"
        );
        assert_eq!(
            cmd.get_env("ZDOTDIR").and_then(|value| value.to_str()),
            Some("/tmp/tty7-zdotdir-test")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn detected_shell_override_uses_explicit_command_builder() {
        let portable_shell = CommandBuilder::new_default_prog().get_shell();
        let detected_shell = format!("{portable_shell}-detected");
        let cmd = default_prog_with_override(Some(detected_shell.clone()));

        assert!(!cmd.is_default_prog());
        let argv: Vec<_> = cmd
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(argv, vec![detected_shell]);
    }

    #[cfg(not(windows))]
    #[test]
    fn no_detected_shell_keeps_portable_login_default() {
        let cmd = default_prog_with_override(None);

        assert!(cmd.is_default_prog());
        assert!(!default_shell_name(&cmd).is_empty());
    }

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

    /// The foreground-command predicate: only a process group *other* than the
    /// shell counts as a running command; matching pids, or missing data, mean
    /// the shell is idle at its own prompt (so we never suppress a real prompt).
    #[test]
    fn foreground_command_distinguishes_the_shell_from_a_command() {
        // Shell idle at its prompt: the shell is the PTY's foreground group.
        assert!(!is_foreground_command(Some(1000), Some(1000)));
        // A command (ssh, vim, a nested shell) owns the PTY: a different group.
        assert!(is_foreground_command(Some(2000), Some(1000)));
        // Unknown foreground group, unknown shell pid, or a non-positive pgid all
        // answer "shell is foreground" — a bad reading must not disengage editing.
        assert!(!is_foreground_command(None, Some(1000)));
        assert!(!is_foreground_command(Some(2000), None));
        assert!(!is_foreground_command(Some(0), Some(1000)));
    }

    /// The reader's gate for issue #26: a remote shell over ssh emits its own
    /// OSC 133 marks, which the sniffer reads as "at prompt" — but because a
    /// foreground command (ssh) owns the PTY, the reader drops that flag so
    /// tty7's local line editor stays disengaged and Tab reaches the remote shell.
    #[test]
    fn foreground_program_prompt_marks_do_not_claim_the_prompt() {
        let mut s = OscSniffer::new();
        // The remote fish draws its prompt: A (start) then B (input begins).
        let mut signals = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        assert!(
            signals.shell.as_ref().unwrap().at_prompt,
            "the raw marks read as at-prompt"
        );

        // The reader consults the foreground gate before reporting. With ssh (a
        // different process group) on the PTY, the prompt flag is cleared.
        let ssh_running = is_foreground_command(Some(2000), Some(1000));
        if signals.shell.as_ref().is_some_and(|st| st.at_prompt) && ssh_running {
            signals.shell.as_mut().unwrap().at_prompt = false;
        }
        assert!(
            !signals.shell.as_ref().unwrap().at_prompt,
            "a foreground program's prompt marks must not engage the local editor"
        );

        // Sanity: the very same marks with the shell itself foreground (idle at a
        // local prompt) keep at_prompt true — the local editor still engages.
        let mut local = s.feed(b"\x1b]133;A\x1b]133;B\x07");
        let shell_idle = is_foreground_command(Some(1000), Some(1000));
        if local.shell.as_ref().is_some_and(|st| st.at_prompt) && shell_idle {
            local.shell.as_mut().unwrap().at_prompt = false;
        }
        assert!(local.shell.as_ref().unwrap().at_prompt);
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
            remote: None,
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

    #[test]
    fn attach_replays_initial_cwd_even_before_shell_reports_osc7() {
        let mut st = test_state(true);
        st.cwd = Some(PathBuf::from("/Users/alice/clone/tty7"));

        let (tx, rx) = mpsc::channel();
        attach_subscriber(&mut st, tx);

        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Size(_))));
        assert!(matches!(rx.try_recv(), Ok(DaemonMsg::Snapshot(_))));
        assert!(
            matches!(rx.try_recv(), Ok(DaemonMsg::Cwd(p)) if p == PathBuf::from("/Users/alice/clone/tty7"))
        );
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
            || false, // no PTY here → treat the shell as foreground
            || None,
            Arc::new(DeathReporter::new(move || {
                dead_flag.store(true, Ordering::SeqCst)
            })),
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
            || false,
            || None,
            Arc::new(DeathReporter::new(move || dead_tx.send(()).unwrap())),
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
            || false,
            || None,
            Arc::new(DeathReporter::new(move || {
                dead_flag.store(true, Ordering::SeqCst)
            })),
        );
        handle.join().unwrap();

        assert!(!state.lock().unwrap().alive);
        assert!(!dead.load(Ordering::SeqCst));
    }

    /// The latch that lets the reader's EOF and (on Windows) the child-exit
    /// monitor both report the same death without the subscriber seeing two
    /// `Exited`s: the first `report` notifies, the second is a silent no-op.
    #[test]
    fn death_reporter_notifies_once_across_racing_callers() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let (sub_tx, sub_rx) = mpsc::channel();
        state.lock().unwrap().subscriber = Some(sub_tx);
        let shutting_down = AtomicBool::new(false);
        let calls = Arc::new(AtomicBool::new(false));
        let calls_flag = calls.clone();
        let death = DeathReporter::new(move || calls_flag.store(true, Ordering::SeqCst));

        // Two reporters (stand-ins for the reader and the monitor) both fire.
        death.report(&state, &shutting_down);
        death.report(&state, &shutting_down);

        assert!(!state.lock().unwrap().alive);
        // Exactly one `Exited`, then nothing more.
        assert!(matches!(
            sub_rx.try_recv(),
            Ok(DaemonMsg::Exited { code: None })
        ));
        assert!(
            sub_rx.try_recv().is_err(),
            "a second report must not re-notify"
        );
    }

    /// With nobody attached, the *first* report hands the pane to `on_dead` and a
    /// racing second report neither re-fires it nor panics on the taken `FnOnce`.
    #[test]
    fn death_reporter_fires_on_dead_at_most_once() {
        let state = Arc::new(Mutex::new(test_state(true)));
        let shutting_down = AtomicBool::new(false);
        let (dead_tx, dead_rx) = mpsc::channel();
        let death = DeathReporter::new(move || dead_tx.send(()).unwrap());

        death.report(&state, &shutting_down);
        death.report(&state, &shutting_down);

        assert!(dead_rx.try_recv().is_ok(), "unattached death → on_dead");
        assert!(dead_rx.try_recv().is_err(), "on_dead must fire only once");
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

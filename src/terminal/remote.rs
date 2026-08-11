#![allow(dead_code)]

use std::borrow::Cow;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event as AlacEvent, EventListener};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{self, CursorShape, CursorStyle};

use crate::terminal::parked_cursor::{CursorCut, ParkedCursorRepair, ParkedCursorScanner};

use std::collections::VecDeque;

use crate::core::cli_agent::{AgentSessionState, CLIAgent};
use crate::core::config::CursorStyle as ConfigCursorStyle;
use crate::core::osc::OscTokenizer;
use crate::daemon::protocol::{
    AuthPromptKind, AuthResponse, ClientMsg, DaemonMsg, KnownHostEntry, KnownHostId,
    LoopbackForward, LoopbackForwardId, LoopbackForwardInfo, LoopbackForwardRequest,
    ManagedForward, NativeSshSpec, PaneProcs, RemoteContext, RestoreFrom, SftpEntry,
    SftpJobProgress, SftpOp, SftpOpResult, SftpTransferSpec, ShellSpec, SshForwardRule, SshPhase,
    WinSize, WorkspaceOp, WorkspaceRequest,
};
use crate::daemon::transport::{self, Stream};
use gpui::EntityId;

use super::size::TermSize;

#[derive(Clone)]
pub struct EventProxy {
    tx: smol::channel::Sender<AlacEvent>,
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
        let _ = self.tx.try_send(event);
    }
}

#[derive(Default, Clone, Copy)]
struct ShellState {
    active: bool,
    at_prompt: bool,
    last_exit: Option<i32>,
    seq: u64,
    cycle: u64,
}

struct ReaderSignals {
    cwd: Arc<Mutex<Option<PathBuf>>>,
    shell: Arc<Mutex<ShellState>>,
    remote: Arc<Mutex<Option<RemoteContext>>>,
    agent: Arc<Mutex<Option<CLIAgent>>>,
    agent_session: Arc<Mutex<Option<AgentSessionState>>>,
    exited: Arc<AtomicBool>,
    child_exited: Arc<AtomicBool>,
    zle_reading: Arc<AtomicBool>,
    shell_vi_mode: Arc<AtomicBool>,
    running_command: Arc<Mutex<String>>,
    auth: Arc<Mutex<VecDeque<(u64, AuthPromptKind)>>>,
    phase: Arc<Mutex<Option<SshPhase>>>,
    /// Kitty-graphics images the daemon lifted out of the stream (issue #213),
    /// anchored to the grid for the paint path to blit. Shared with the reader,
    /// which places/deletes them as `DaemonMsg::Image`/`DeleteImage` frames land.
    images: crate::terminal::images::ImageStore,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaneWorkspace {
    pub workspace: crate::core::session::WorkspaceId,
    pub target: crate::core::session::RemoteTarget,
    pub spec: Option<Box<NativeSshSpec>>,
}

impl PaneWorkspace {
    pub fn shares_localhost(&self) -> bool {
        matches!(self.target, crate::core::session::RemoteTarget::Wsl { .. })
    }

    pub fn route_header(&self) -> anyhow::Result<crate::daemon::router::RouteHeader> {
        use crate::core::session::RemoteTarget;
        use crate::daemon::router::RouteHeader;
        let header = match (&self.target, &self.spec) {
            (RemoteTarget::Wsl { distro }, _) => RouteHeader::wsl(distro.clone()),
            (RemoteTarget::LocalStdio { program, args }, _) => {
                let mut argv: Vec<&str> = args.iter().map(String::as_str).collect();
                if !argv.contains(&"--pane") {
                    argv.push("--pane");
                }
                RouteHeader::local_stdio(program.clone(), &argv)
            }
            (_, Some(spec)) => RouteHeader::ssh((**spec).clone()),
            (target, None) => {
                return Err(anyhow::anyhow!(
                    "this workspace has no SSH connection details ({target:?}), so its panes \
                     cannot be routed"
                ));
            }
        };
        Ok(header.for_pane())
    }
}

#[derive(Clone, Debug, Default)]
pub enum PaneRoute {
    #[default]
    Local,
    Remote(Box<crate::daemon::router::RouteHeader>),
    Unroutable(String),
}

impl PaneRoute {
    pub fn for_workspace(workspace: Option<&PaneWorkspace>) -> PaneRoute {
        match workspace {
            None => PaneRoute::Local,
            Some(ws) => match ws.route_header() {
                Ok(header) => PaneRoute::Remote(Box::new(header)),
                Err(e) => PaneRoute::Unroutable(e.to_string()),
            },
        }
    }

    pub fn header(&self) -> Option<&crate::daemon::router::RouteHeader> {
        match self {
            PaneRoute::Remote(header) => Some(header),
            PaneRoute::Local | PaneRoute::Unroutable(_) => None,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, PaneRoute::Local)
    }
}

pub struct RemoteTerminal {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub events: smol::channel::Receiver<AlacEvent>,
    pub palette: [alacritty_terminal::vte::ansi::Rgb; 256],
    pub exited: bool,
    size: TermSize,
    synced_size: bool,
    /// The `(cell_w, cell_h)` last sent to the daemon, in device pixels. Tracked
    /// alongside `size` so a display-scale change still reaches the child even
    /// when the grid dimensions are unchanged.
    synced_cell: (u16, u16),
    writer: Mutex<Stream>,
    cwd: Arc<Mutex<Option<PathBuf>>>,
    shell_state: Arc<Mutex<ShellState>>,
    remote_context: Arc<Mutex<Option<RemoteContext>>>,
    exited_flag: Arc<AtomicBool>,
    child_exited: Arc<AtomicBool>,
    zle_reading: Arc<AtomicBool>,
    shell_vi_mode: Arc<AtomicBool>,
    /// The line the shell said it is running, from the OSC 133;C mark, still
    /// percent-escaped as the integration sent it. Empty while at a prompt.
    ///
    /// This is the only place the *text* of a running command exists on the
    /// client: the frame the daemon sends says a command runs, not which one.
    /// The close confirmation has to name what it is about to end.
    running_command: Arc<Mutex<String>>,
    auth_prompts: Arc<Mutex<VecDeque<(u64, AuthPromptKind)>>>,
    ssh_phase: Arc<Mutex<Option<SshPhase>>>,
    ssh_endpoint: Option<(String, u16)>,
    /// The account the SSH connection authenticates as. `ssh_endpoint` is what
    /// the disconnect strip and the forward sheet need; the keychain files a
    /// password under the user as well, so the auth sheet needs this too.
    ssh_user: Option<String>,
    auto_supplied_password: bool,
    agent: Arc<Mutex<Option<CLIAgent>>>,
    agent_session: Arc<Mutex<Option<AgentSessionState>>>,
    /// Kitty-graphics images placed on this pane's grid (issue #213).
    /// Written by the reader thread from out-of-band `Image`/`DeleteImage`
    /// frames, read by the paint path — only the client holds the grid the
    /// anchors are relative to, so the store lives here rather than in the daemon.
    images: crate::terminal::images::ImageStore,
    route: PaneRoute,
    proxy: EventProxy,
    reader_thread: Option<JoinHandle<()>>,
    /// Tells the *current* reader thread to bow out. Teardown must never wait
    /// for the reader: on Windows, `shutdown()` does not wake a thread parked
    /// in a blocking `read()` on the same socket, so joining it from the UI
    /// thread hangs the whole window whenever the peer has gone silent (the
    /// exact state a zombie SSH leg leaves behind). The reader re-checks this
    /// flag under the term lock before every grid mutation, so once it is set
    /// the abandoned thread can only exit, never write.
    reader_quit: Arc<AtomicBool>,
    /// Test hook: pretend the daemon advertised `FEATURE_RESIZE_ECHO`.
    /// Production code probes the real daemon per call in [`Self::resize`].
    #[cfg(test)]
    force_resize_echo: bool,
}

/// The workspace id a spawn carries, so the pane's shell gets `$TTY7_WS` and a
/// CLI running in it can use workspace-scoped verbs with no argument.
///
/// This is the same id as `owner`, but decided separately: `owner` is also
/// gated on FEATURE_PANE_OWNER, while the workspace field rides the c4p5 spawn
/// kind and needs no feature probe. Local routes only — a remote server keeps
/// its own machine tree, and this id names a workspace in ours.
fn spawn_workspace(owner: Option<&str>, route: &PaneRoute) -> Option<String> {
    owner.filter(|_| route.is_local()).map(|id| id.to_string())
}

impl RemoteTerminal {
    pub fn spawn(
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cwd: Option<PathBuf>,
        shell: Option<ShellSpec>,
    ) -> anyhow::Result<(Self, u64)> {
        Self::spawn_on(
            &PaneRoute::Local,
            size,
            cell_w,
            cell_h,
            cwd,
            shell,
            None,
            None,
        )
    }

    pub fn spawn_on(
        route: &PaneRoute,
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cwd: Option<PathBuf>,
        shell: Option<ShellSpec>,
        owner: Option<String>,
        restore: Option<RestoreFrom>,
    ) -> anyhow::Result<(Self, u64)> {
        let retry_cwd = cwd.clone();
        let retry_shell = shell.clone();
        let retry_owner = owner.clone();
        let retry_restore = restore.clone();
        match Self::spawn_once(route, size, cell_w, cell_h, cwd, shell, owner, restore) {
            Ok(term) => Ok(term),
            Err(first_err) if daemon_not_listening(&first_err) => {
                if let Err(start_err) = crate::daemon::spawn::ensure_running() {
                    return Err(anyhow::anyhow!(
                        "daemon not running ({first_err}); starting one failed: {start_err}"
                    ));
                }
                Self::spawn_once(route, size, cell_w, cell_h, retry_cwd, retry_shell, retry_owner, retry_restore)
                    .map_err(|second_err| {
                        anyhow::anyhow!(
                            "daemon not running ({first_err}); started one but Spawn still failed: {second_err}"
                        )
                    })
            }
            Err(first_err)
                if route.is_local() && daemon_disconnected_before_spawn_reply(&first_err) =>
            {
                if let Err(restart_err) = crate::daemon::spawn::restart() {
                    return Err(anyhow::anyhow!(
                        "daemon disconnected before Spawn reply ({first_err}); restart failed: {restart_err}"
                    ));
                }
                Self::spawn_once(route, size, cell_w, cell_h, retry_cwd, retry_shell, retry_owner, retry_restore).map_err(|second_err| {
                    anyhow::anyhow!(
                        "daemon disconnected before Spawn reply ({first_err}); restarted daemon but Spawn still failed: {second_err}"
                    )
                })
            }
            Err(err) => Err(err),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_once(
        route: &PaneRoute,
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cwd: Option<PathBuf>,
        shell: Option<ShellSpec>,
        owner: Option<String>,
        restore: Option<RestoreFrom>,
    ) -> anyhow::Result<(Self, u64)> {
        let mut stream = connect_routed(route)?;
        let win = win_size(size, cell_w, cell_h);

        let workspace = spawn_workspace(owner.as_deref(), route);
        let spawned_in = cwd.clone();

        let owner = owner.filter(|_| {
            route.is_local()
                && crate::daemon::spawn::local_daemon_supports(
                    crate::daemon::protocol::FEATURE_PANE_OWNER,
                )
        });

        // Only the local daemon's feature list is known here; a routed spawn
        // reaches a server whose build we have not asked about, and one that
        // predates the field would silently drop it. Sending it anyway would
        // cost nothing but would make the log claim a restore that never
        // happened, so the local case is the only one that asks.
        let restore = restore.and_then(|want| {
            let local = route.is_local();
            let supported = crate::daemon::spawn::local_daemon_supports(
                crate::daemon::protocol::FEATURE_RESTORE_SCROLLBACK,
            );
            if local && supported {
                return Some(want);
            }
            // Said out loud, because the symptom of dropping it here is a pane
            // that opens blank — which is also what a pane with nothing stored
            // looks like, and what a daemon that refused would produce. Three
            // causes, one appearance; without this line the only way to tell
            // them apart is to read the source.
            log::info!(
                "not asking to restore pane {}'s screen: local={local} \
                 daemon-supports-restore={supported}",
                want.pane_id
            );
            None
        });

        ClientMsg::Spawn {
            cwd,
            size: win,
            shell,
            owner,
            workspace,
            restore,
        }
        .encode(&mut stream)?;
        let pane_id = match DaemonMsg::read(&mut stream)? {
            DaemonMsg::Spawned { pane_id } => pane_id,
            // Passed through, not wrapped: the caller already logs which
            // spawn this was, and the window shows only this text.
            DaemonMsg::Error(msg) => return Err(anyhow::anyhow!(msg)),
            other => {
                return Err(anyhow::anyhow!(
                    "unexpected daemon reply to Spawn: {other:?}"
                ));
            }
        };

        let mut term = Self::from_stream(stream, size)?;
        term.route = route.clone();
        term.seed_cwd(spawned_in);
        Ok((term, pane_id))
    }

    /// Remember the directory the daemon was asked to spawn in, so everything
    /// that keys off a pane's cwd — the sidebar's repo grouping above all —
    /// has an answer before the shell gets far enough to report its own via
    /// OSC 7. The reader thread is already running, so a report that beat us
    /// here wins: it describes where the shell actually landed, which is not
    /// always where we asked (a missing directory sends the daemon home, an
    /// rc file may `cd` on its own).
    fn seed_cwd(&self, cwd: Option<PathBuf>) {
        let Some(cwd) = cwd else { return };
        if let Ok(mut guard) = self.cwd.lock() {
            guard.get_or_insert(cwd);
        }
    }

    pub fn attach(size: TermSize, cell_w: u16, cell_h: u16, pane_id: u64) -> anyhow::Result<Self> {
        Self::attach_on(&PaneRoute::Local, size, cell_w, cell_h, pane_id)
    }

    pub fn attach_on(
        route: &PaneRoute,
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        pane_id: u64,
    ) -> anyhow::Result<Self> {
        let mut stream = connect_routed(route)?;
        let win = win_size(size, cell_w, cell_h);

        ClientMsg::Attach { pane_id, size: win }.encode(&mut stream)?;
        let buffered = attach_reply_prefix(&mut stream, pane_id, attach_reply_wait(route))?;
        let mut term = Self::from_stream_with(stream, size, buffered)?;
        term.route = route.clone();
        Ok(term)
    }

    pub fn open_relink(
        route: &PaneRoute,
        pane_id: u64,
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
    ) -> anyhow::Result<Stream> {
        let mut stream = connect_routed(route)?;
        ClientMsg::Attach {
            pane_id,
            size: win_size(size, cell_w, cell_h),
        }
        .encode(&mut stream)?;
        Ok(stream)
    }

    pub fn adopt_relink(
        &mut self,
        stream: Stream,
        route: &PaneRoute,
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
    ) -> anyhow::Result<()> {
        self.stop_reader();
        while self.events.try_recv().is_ok() {}

        let read_half = stream.try_clone()?;

        self.exited_flag.store(false, Ordering::SeqCst);
        self.exited = false;
        {
            use alacritty_terminal::vte::ansi::Handler as _;
            let mut term = self.term.lock();
            term.reset_state();
        }
        // The grid was just reset, so every image anchor now points nowhere.
        // Drop them; the daemon does not replay out-of-band image frames, so a
        // browser redraws on its next transmit (see issue #213's reattach note).
        self.images.clear();

        let quit = Arc::new(AtomicBool::new(false));
        let reader = Self::spawn_reader(
            self.term.clone(),
            self.proxy.clone(),
            read_half,
            Vec::new(),
            quit.clone(),
            ReaderSignals {
                cwd: self.cwd.clone(),
                shell: self.shell_state.clone(),
                remote: self.remote_context.clone(),
                agent: self.agent.clone(),
                agent_session: self.agent_session.clone(),
                exited: self.exited_flag.clone(),
                child_exited: self.child_exited.clone(),
                zle_reading: self.zle_reading.clone(),
                shell_vi_mode: self.shell_vi_mode.clone(),
                running_command: self.running_command.clone(),
                auth: self.auth_prompts.clone(),
                phase: self.ssh_phase.clone(),
                images: self.images.clone(),
            },
        );
        if let Ok(mut writer) = self.writer.lock() {
            *writer = stream;
        }
        self.reader_thread = Some(reader);
        self.reader_quit = quit;
        self.route = route.clone();
        self.synced_size = false;
        self.resize(size, cell_w, cell_h);
        Ok(())
    }

    pub(super) fn from_stream(stream: Stream, size: TermSize) -> anyhow::Result<Self> {
        Self::from_stream_with(stream, size, Vec::new())
    }

    pub(super) fn from_stream_with(
        stream: Stream,
        size: TermSize,
        buffered: Vec<u8>,
    ) -> anyhow::Result<Self> {
        let read_half = stream.try_clone()?;
        let write_half = stream;

        let (tx, rx) = smol::channel::unbounded();
        let proxy = EventProxy {
            tx,
            replaying: Arc::new(AtomicBool::new(false)),
        };

        let user_config = crate::core::config::Config::load();
        let config = terminal_config_from_user(&user_config);
        let term = Term::new(config, &size, proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let cwd: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let shell_state: Arc<Mutex<ShellState>> = Arc::new(Mutex::new(ShellState::default()));
        let remote_context: Arc<Mutex<Option<RemoteContext>>> = Arc::new(Mutex::new(None));
        let agent: Arc<Mutex<Option<CLIAgent>>> = Arc::new(Mutex::new(None));
        let agent_session: Arc<Mutex<Option<AgentSessionState>>> = Arc::new(Mutex::new(None));
        let exited_flag = Arc::new(AtomicBool::new(false));
        let child_exited = Arc::new(AtomicBool::new(false));
        let zle_reading = Arc::new(AtomicBool::new(false));
        let shell_vi_mode = Arc::new(AtomicBool::new(false));
        let running_command: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let auth_prompts: Arc<Mutex<VecDeque<(u64, AuthPromptKind)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let ssh_phase: Arc<Mutex<Option<SshPhase>>> = Arc::new(Mutex::new(None));
        let images = crate::terminal::images::ImageStore::new();

        let reader_quit = Arc::new(AtomicBool::new(false));
        let reader_thread = Self::spawn_reader(
            term.clone(),
            proxy.clone(),
            read_half,
            buffered,
            reader_quit.clone(),
            ReaderSignals {
                cwd: cwd.clone(),
                shell: shell_state.clone(),
                remote: remote_context.clone(),
                agent: agent.clone(),
                agent_session: agent_session.clone(),
                exited: exited_flag.clone(),
                child_exited: child_exited.clone(),
                zle_reading: zle_reading.clone(),
                shell_vi_mode: shell_vi_mode.clone(),
                running_command: running_command.clone(),
                auth: auth_prompts.clone(),
                phase: ssh_phase.clone(),
                images: images.clone(),
            },
        );

        Ok(Self {
            term,
            events: rx,
            palette: super::palette::build(),
            exited: false,
            size,
            synced_size: false,
            synced_cell: (0, 0),
            writer: Mutex::new(write_half),
            cwd,
            shell_state,
            remote_context,
            exited_flag,
            child_exited,
            zle_reading,
            shell_vi_mode,
            running_command,
            auth_prompts,
            ssh_phase,
            ssh_endpoint: None,
            ssh_user: None,
            auto_supplied_password: false,
            agent,
            agent_session,
            images,
            route: PaneRoute::Local,
            proxy,
            reader_thread: Some(reader_thread),
            reader_quit,
            #[cfg(test)]
            force_resize_echo: false,
        })
    }

    pub fn detach_link(&mut self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = ClientMsg::Detach.encode(&mut *writer);
        }
        self.stop_reader();
        self.poll_exited();
    }

    /// Retires the current reader thread without waiting for it. Joining is
    /// not an option here: the callers run on the UI thread, and on Windows
    /// `shutdown()` does not wake a reader parked in a blocking `read()`, so
    /// against a silent peer the join would hang the whole window (the reader
    /// only unblocks when the peer eventually closes — which a zombie SSH leg
    /// never does). Instead the reader is told to quit and abandoned; it wakes
    /// within one read-timeout tick, sees the flag, and exits without ever
    /// touching the grid again.
    fn stop_reader(&mut self) {
        self.reader_quit.store(true, Ordering::SeqCst);
        if let Ok(writer) = self.writer.lock() {
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
        drop(self.reader_thread.take());
    }

    pub fn apply_user_config(&self, user_config: &crate::core::config::Config) {
        let mut term = self.term.lock();
        term.set_options(terminal_config_from_user(user_config));
    }

    /// Whether this build puts back the cursor a repaint parked — see
    /// [`crate::terminal::parked_cursor`].
    ///
    /// Only conhost parks one, so like `conpty_resize` the repair is Windows'
    /// alone. On a raw pty the application owns the cursor and is free to end a
    /// repaint on the text it just wrote and then echo the next keystroke
    /// straight after it, with no positioning of its own: vim opens its command
    /// line that way, and putting the cursor back on the cell the repaint hid it
    /// on drops the `wq!` typed next onto the row being edited (#430).
    const REPAIR_PARKED_CURSOR: bool = cfg!(windows);

    fn spawn_reader(
        term: Arc<FairMutex<Term<EventProxy>>>,
        proxy: EventProxy,
        read_half: Stream,
        buffered: Vec<u8>,
        quit: Arc<AtomicBool>,
        signals: ReaderSignals,
    ) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("tty7-remote-reader".to_string())
            .spawn(move || {
                let ReaderSignals {
                    cwd,
                    shell,
                    remote,
                    agent,
                    agent_session,
                    exited: exited_flag,
                    child_exited,
                    zle_reading,
                    shell_vi_mode,
                    running_command,
                    auth,
                    phase,
                    images,
                } = signals;
                crate::core::threads::promote_to_user_interactive();
                let mut stream = read_half;
                let mut processor: ansi::Processor = ansi::Processor::new();
                let mut osc = OscNotifyScanner::default();
                let mut mode_tok = OscTokenizer::new(&[b"133"]);
                let mut zle_tok = OscTokenizer::new(&[b"133"]);
                let mut cursor_scan = ParkedCursorScanner::new();
                let mut parked_cursor = ParkedCursorRepair::default();
                let mut pending: Vec<u8> = buffered;
                // Kitty-graphics decode runs on its own thread with newest-frame
                // coalescing (issue #213): inflating a full-window browser frame
                // is ~42 ms, and doing it inline here would block PTY output and
                // scrolling for that long every frame. The worker owns the inflate
                // + BGRA swap + placement; the reader only captures the grid
                // anchor and hands off the still-compressed frame. Dropped when
                // the loop ends, which joins the thread.
                let image_decoder = {
                    let proxy = proxy.clone();
                    crate::terminal::images::DecodeWorker::spawn(images.clone(), move || {
                        proxy.send_event(AlacEvent::Wakeup);
                    })
                };
                let mut scratch = vec![0u8; 256 * 1024];

                let trace = std::env::var("TTY7_TRACE").is_ok_and(|v| !v.is_empty() && v != "0");
                let mut tr_last = std::time::Instant::now();
                let mut tr_bytes: u64 = 0;
                let mut tr_reads: u32 = 0;
                let mut tr_read_t = std::time::Duration::ZERO;
                let mut tr_lock_t = std::time::Duration::ZERO;
                let mut tr_adv_t = std::time::Duration::ZERO;
                let mut tr_frames: u32 = 0;

                let teardown = || {
                    // A retired reader exits silently: the pane is being
                    // relinked or released, not dying — marking it exited
                    // would wrongly kill the freshly adopted link.
                    if quit.load(Ordering::SeqCst) {
                        return;
                    }
                    term.lock().exit();
                    exited_flag.store(true, Ordering::SeqCst);
                    proxy.send_event(AlacEvent::Wakeup);
                    proxy.send_event(AlacEvent::Exit);
                };

                let mut out_batch: Vec<u8> = Vec::new();

                'main: loop {
                    macro_rules! flush_batch {
                        () => {
                            if !out_batch.is_empty() {
                                // The scanner reports an offset one past the
                                // sequence it matched, in ascending order, so the
                                // batch splits at each of them: advance the
                                // emulator to the cut, act on the state that
                                // sequence left behind, carry on.
                                let mut cuts: Vec<(usize, CursorCut)> = Vec::new();
                                if Self::REPAIR_PARKED_CURSOR {
                                    cursor_scan.feed(&out_batch, |off, c| cuts.push((off, c)));
                                }
                                {
                                    let t0 = trace.then(std::time::Instant::now);
                                    let mut term = term.lock();
                                    if quit.load(Ordering::SeqCst) {
                                        return;
                                    }
                                    let t1 = trace.then(std::time::Instant::now);
                                    if cuts.is_empty() {
                                        processor.advance(&mut *term, &out_batch);
                                    } else {
                                        let mut at = 0usize;
                                        for (off, cut) in cuts {
                                            processor.advance(&mut *term, &out_batch[at..off]);
                                            at = off;
                                            parked_cursor.apply(&mut term, cut);
                                        }
                                        processor.advance(&mut *term, &out_batch[at..]);
                                    }
                                    if let (Some(t0), Some(t1)) = (t0, t1) {
                                        tr_lock_t += t1 - t0;
                                        tr_adv_t += t1.elapsed();
                                    }
                                }
                                let mut notes = Vec::new();
                                osc.feed(&out_batch, &mut notes);
                                for (title, body) in notes {
                                    notify_desktop(title.as_deref(), &body);
                                }
                                mode_tok.feed(&out_batch, |payload| {
                                    if let Some(mode) = payload.strip_prefix(b"133;V;") {
                                        shell_vi_mode.store(
                                            mode.first() == Some(&b'1'),
                                            Ordering::Relaxed,
                                        );
                                    }
                                });
                                zle_tok.feed(&out_batch, |payload| {
                                    if let Some(mark) = payload.strip_prefix(b"133;") {
                                        match mark.first() {
                                            Some(b'B') => {
                                                zle_reading.store(true, Ordering::Relaxed);
                                                // Back at a prompt: whatever
                                                // ran is over, so the name of
                                                // it must not outlive it into
                                                // the next close question.
                                                if let Ok(mut cmd) = running_command.lock() {
                                                    cmd.clear();
                                                }
                                            }
                                            Some(b'V') => {
                                                shell_vi_mode.store(
                                                    mark.strip_prefix(b"V;")
                                                        .is_some_and(|v| v.first() == Some(&b'1')),
                                                    Ordering::Relaxed,
                                                );
                                            }
                                            Some(b'C') => {
                                                zle_reading.store(false, Ordering::Relaxed);
                                                // `C` alone is a command start
                                                // with no line to report — the
                                                // PowerShell path sends that —
                                                // and leaves the field empty
                                                // rather than holding a stale
                                                // one.
                                                let line = mark
                                                    .strip_prefix(b"C;")
                                                    .map(|c| String::from_utf8_lossy(c).into_owned())
                                                    .unwrap_or_default();
                                                if let Ok(mut cmd) = running_command.lock() {
                                                    *cmd = line;
                                                }
                                            }
                                            _ => zle_reading.store(false, Ordering::Relaxed),
                                        }
                                    }
                                });
                                proxy.send_event(AlacEvent::Wakeup);
                                out_batch.clear();
                            }
                        };
                    }

                    loop {
                        // Checked per frame, not just per read: a retired
                        // reader may still hold complete frames in `pending`,
                        // and every arm below writes shared state that
                        // outlives a relink (cwd, prompt, agent — and
                        // `Exited`, which would close the freshly adopted
                        // pane via `child_exited`).
                        if quit.load(Ordering::SeqCst) {
                            return;
                        }
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
                            // Geometry, applied at this exact stream position.
                            // During replay each ring segment is preceded by
                            // its Size; live, the daemon echoes one when it
                            // applies our Resize (FEATURE_RESIZE_ECHO), which
                            // is the point in the stream where the bytes stop
                            // being old-width — everything still queued before
                            // this frame must be parsed into the old grid.
                            DaemonMsg::Size(ws) => {
                                flush_batch!();
                                {
                                    let mut term = term.lock();
                                    if quit.load(Ordering::SeqCst) {
                                        return;
                                    }
                                    term.resize(TermSize::new(
                                        ws.cols as usize,
                                        ws.rows as usize,
                                    ));
                                }
                                proxy.send_event(AlacEvent::Wakeup);
                            }
                            DaemonMsg::Snapshot(bytes) => {
                                flush_batch!();
                                cursor_scan.reset();
                                parked_cursor.reset();
                                proxy.replaying.store(true, Ordering::Relaxed);
                                {
                                    let mut term = term.lock();
                                    if quit.load(Ordering::SeqCst) {
                                        return;
                                    }
                                    processor.advance(&mut *term, &bytes);
                                    if processor.sync_timeout().sync_timeout().is_some() {
                                        processor.stop_sync(&mut *term);
                                    }
                                }
                                mode_tok.feed(&bytes, |payload| {
                                    if let Some(mode) = payload.strip_prefix(b"133;V;") {
                                        shell_vi_mode.store(
                                            mode.first() == Some(&b'1'),
                                            Ordering::Relaxed,
                                        );
                                    }
                                });
                                proxy.replaying.store(false, Ordering::Relaxed);
                                proxy.send_event(AlacEvent::Wakeup);
                            }
                            DaemonMsg::Output(bytes) => {
                                out_batch.extend_from_slice(&bytes);
                                tr_frames += 1;
                            }
                            // Kitty graphics (issue #213): the daemon lifted an
                            // image out of the stream and forwarded it out-of-band,
                            // interleaved *in stream order* with the Output frames
                            // around it. Flush the pending text first so the grid
                            // cursor sits where the sender drew the image, then
                            // anchor the placement to that cell in scroll-stable
                            // absolute-row coordinates (`history_size -
                            // display_offset + cursor_line`), so it tracks
                            // scrolling.
                            DaemonMsg::Image(frame) => {
                                flush_batch!();
                                if let Some(img) =
                                    tty7_core::core::kitty_graphics::Image::decode_frame_owned(
                                        frame,
                                    )
                                {
                                    // Capture the anchor *now*, at the cursor cell
                                    // the transmission arrived on; the decode is
                                    // deferred to the worker thread but must land
                                    // at this position, not wherever the cursor has
                                    // scrolled to by the time inflate finishes.
                                    let (anchor_row, anchor_col) = {
                                        use alacritty_terminal::grid::Dimensions as _;
                                        let term = term.lock();
                                        if quit.load(Ordering::SeqCst) {
                                            return;
                                        }
                                        let grid = term.grid();
                                        let row = grid.history_size() as i64
                                            - grid.display_offset() as i64
                                            + i64::from(grid.cursor.point.line.0);
                                        (row, grid.cursor.point.column.0)
                                    };
                                    // Hand off without blocking the reader: the
                                    // worker inflates, swaps, places, and wakes the
                                    // view. Stale frames coalesce away there.
                                    image_decoder.submit(
                                        crate::terminal::images::PendingFrame {
                                            img,
                                            anchor_row,
                                            anchor_col,
                                        },
                                    );
                                }
                            }
                            // An `a=d` delete, lifted out the same way. Order with
                            // the surrounding output does not matter for a delete
                            // (it targets by id/placement, not cursor position),
                            // but flushing keeps a delete-then-retransmit in the
                            // same read from racing its own replacement.
                            DaemonMsg::DeleteImage(sel) => {
                                flush_batch!();
                                if let Some(del) =
                                    tty7_core::core::kitty_graphics::ImageDelete::decode(&sel)
                                {
                                    image_decoder.delete(&del);
                                    proxy.send_event(AlacEvent::Wakeup);
                                }
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
                                        seq: guard.seq + 1,
                                        cycle: guard.cycle
                                            + u64::from(at_prompt && !guard.at_prompt),
                                    };
                                }
                                if active && at_prompt {
                                    let mut term = term.lock();
                                    if quit.load(Ordering::SeqCst) {
                                        return;
                                    }
                                    let resets = stale_mode_resets(*term.mode());
                                    if !resets.is_empty() {
                                        processor.advance(&mut *term, &resets);
                                        drop(term);
                                        proxy.send_event(AlacEvent::Wakeup);
                                    }
                                }
                            }
                            DaemonMsg::RemoteContext(ctx) => {
                                flush_batch!();
                                if let Ok(mut guard) = cwd.lock() {
                                    *guard = None;
                                }
                                if let Ok(mut guard) = remote.lock() {
                                    *guard = ctx;
                                }
                            }
                            DaemonMsg::AuthPrompt { request_id, prompt } => {
                                flush_batch!();
                                if let Ok(mut guard) = auth.lock() {
                                    guard.push_back((request_id, prompt));
                                }
                                proxy.send_event(AlacEvent::Wakeup);
                            }
                            DaemonMsg::SshStatus { phase: p } => {
                                flush_batch!();
                                if let Ok(mut guard) = phase.lock() {
                                    *guard = Some(p);
                                }
                                proxy.send_event(AlacEvent::Wakeup);
                            }
                            DaemonMsg::Agent(a) => {
                                flush_batch!();
                                if let Ok(mut guard) = agent.lock() {
                                    *guard = a;
                                }
                            }
                            DaemonMsg::AgentStatus(state) => {
                                flush_batch!();
                                if let Ok(mut guard) = agent_session.lock() {
                                    *guard = state;
                                }
                                proxy.send_event(AlacEvent::Wakeup);
                            }
                            DaemonMsg::Exited { .. } => {
                                flush_batch!();
                                child_exited.store(true, Ordering::SeqCst);
                                teardown();
                                break 'main;
                            }
                            _ => {}
                        }
                    }
                    flush_batch!();

                    // The read always times out within QUIT_POLL so a reader
                    // parked on a silent link still notices `quit` promptly —
                    // on Windows, `shutdown()` alone would never wake it.
                    const QUIT_POLL: std::time::Duration = std::time::Duration::from_millis(500);
                    let timeout = match processor.sync_timeout().sync_timeout() {
                        Some(deadline) => {
                            let left =
                                deadline.saturating_duration_since(std::time::Instant::now());
                            if left.is_zero() {
                                let mut term = term.lock();
                                if quit.load(Ordering::SeqCst) {
                                    return;
                                }
                                processor.stop_sync(&mut *term);
                                drop(term);
                                proxy.send_event(AlacEvent::Wakeup);
                                continue;
                            }
                            left.min(QUIT_POLL)
                        }
                        None => QUIT_POLL,
                    };
                    let _ = stream.set_read_timeout(Some(timeout));
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

    pub fn poll_exited(&mut self) {
        if self.exited_flag.load(Ordering::SeqCst) {
            self.exited = true;
        }
    }

    pub fn child_exited(&self) -> bool {
        self.child_exited.load(Ordering::SeqCst)
    }

    pub fn write<B: Into<Cow<'static, [u8]>>>(&self, bytes: B) {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return;
        }
        if let Ok(mut writer) = self.writer.lock() {
            let _ = ClientMsg::Input(bytes.into_owned()).encode(&mut *writer);
        }
    }

    /// Whether the daemon behind this pane echoes a `DaemonMsg::Size` into the
    /// output stream when it applies our `ClientMsg::Resize`. When it does, the
    /// local grid reflow is deferred to that echo on the reader thread: any
    /// backlog still queued between daemon and client was produced at the old
    /// geometry, and reflowing before it drains parses old-width bytes into a
    /// new-width grid (maximize during a burst of output garbled the pane).
    ///
    /// Older daemons never echo, so those keep the reflow-at-request-time
    /// behavior. Non-local routes keep it too, for a weaker reason: only the
    /// local daemon's feature set is probed, so the client cannot *rely* on an
    /// echo arriving. A current remote daemon does send one — the router
    /// forwards frames it does not parse, and the reader applies it as a
    /// same-size no-op after the eager reflow — but deferral can't hang on a
    /// maybe. Follow-up: carry the remote daemon's advertised features on the
    /// route handshake and consult them here; remote panes then get the
    /// deferred path for free.
    fn resize_echoed(&self) -> bool {
        #[cfg(test)]
        if self.force_resize_echo {
            return true;
        }
        self.route.is_local()
            && crate::daemon::spawn::local_daemon_supports(
                crate::daemon::protocol::FEATURE_RESIZE_ECHO,
            )
    }

    pub fn resize(&mut self, size: TermSize, cell_w: u16, cell_h: u16) {
        let echoed = self.resize_echoed();
        // The cell size has to be part of the early-out, not just cols/rows: it
        // is reported in *device* pixels, so moving the window between a 2x and
        // a 1x display changes `ws_xpixel`/`ws_ypixel` while the grid stays
        // exactly the same. Comparing only `size` there would skip the resize
        // and leave a pixel-aware child rendering for the old framebuffer.
        let cell = (cell_w, cell_h);
        if self.synced_size && size == self.size && cell == self.synced_cell {
            if echoed {
                // The grid follows the daemon's Size echoes; disagreement here
                // just means an echo is still in flight (or a replay segment is
                // mid-apply), not that the request needs re-sending.
                return;
            }
            use alacritty_terminal::grid::Dimensions as _;
            let term = self.term.lock();
            if term.columns() == size.cols && term.screen_lines() == size.rows {
                return;
            }
        }
        self.synced_size = true;
        self.size = size;
        self.synced_cell = cell;
        if !echoed {
            self.term.lock().resize(size);
        }

        let win = win_size(size, cell_w, cell_h);
        if let Ok(mut writer) = self.writer.lock() {
            let _ = ClientMsg::Resize(win).encode(&mut *writer);
        }
    }

    pub fn foreground_cwd(&self) -> Option<PathBuf> {
        self.cwd.lock().ok().and_then(|g| g.clone())
    }

    pub fn remote_context(&self) -> Option<RemoteContext> {
        self.remote_context.lock().ok().and_then(|g| g.clone())
    }

    pub fn at_prompt(&self) -> bool {
        self.shell_state
            .lock()
            .map(|s| s.active && s.at_prompt)
            .unwrap_or(false)
    }

    pub fn prompt_seq(&self) -> u64 {
        self.shell_state.lock().map(|s| s.seq).unwrap_or(0)
    }

    pub fn prompt_cycle(&self) -> u64 {
        self.shell_state.lock().map(|s| s.cycle).unwrap_or(0)
    }

    pub fn last_exit_code(&self) -> Option<i32> {
        self.shell_state.lock().ok().and_then(|s| s.last_exit)
    }

    pub fn shell_active(&self) -> bool {
        self.shell_state.lock().map(|s| s.active).unwrap_or(false)
    }

    pub fn foreground_agent(&self) -> Option<CLIAgent> {
        self.agent.lock().ok().and_then(|g| *g)
    }

    /// The kitty-graphics image store for this pane. Cheap handle clone — the
    /// store is an `Arc<Mutex<..>>` shared with the reader thread, which places
    /// and deletes images as out-of-band frames arrive from the daemon.
    pub fn images(&self) -> crate::terminal::images::ImageStore {
        self.images.clone()
    }

    pub fn agent_session(&self) -> Option<AgentSessionState> {
        self.agent_session.lock().ok().and_then(|g| g.clone())
    }

    pub fn zle_reading(&self) -> bool {
        self.zle_reading.load(Ordering::Relaxed)
    }

    pub fn shell_vi_mode(&self) -> bool {
        self.shell_vi_mode.load(Ordering::Relaxed)
    }

    /// The line the shell reported running, still percent-escaped. Empty at a
    /// prompt, and empty for a shell whose integration marks the start of a
    /// command without naming it.
    pub fn running_command(&self) -> String {
        self.running_command
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default()
    }

    pub fn size(&self) -> TermSize {
        self.size
    }

    pub fn list_panes() -> Vec<crate::daemon::protocol::PaneInfo> {
        Self::list_panes_on(&PaneRoute::Local)
    }

    pub fn list_panes_on(route: &PaneRoute) -> Vec<crate::daemon::protocol::PaneInfo> {
        Self::try_list_panes_on(route).unwrap_or_default()
    }

    pub fn try_list_panes_on(
        route: &PaneRoute,
    ) -> anyhow::Result<Vec<crate::daemon::protocol::PaneInfo>> {
        let mut stream = connect_routed(route)?;
        ClientMsg::List.encode(&mut stream)?;
        match DaemonMsg::read(&mut stream)? {
            DaemonMsg::PaneList(list) => Ok(list),
            other => Err(anyhow::anyhow!("unexpected reply to List: {other:?}")),
        }
    }

    pub fn kill_pane(pane_id: u64) {
        Self::kill_pane_on(&PaneRoute::Local, pane_id)
    }

    pub fn kill_pane_on(route: &PaneRoute, pane_id: u64) {
        if let Ok(mut stream) = connect_routed(route) {
            let _ = ClientMsg::Kill { pane_id }.encode(&mut stream);
            let _ = stream.shutdown(std::net::Shutdown::Write);
        }
    }

    pub fn ensure_loopback_forward(
        pane_id: u64,
        remote_host: &str,
        remote_port: u16,
    ) -> anyhow::Result<LoopbackForward> {
        let mut stream = connect()?;
        ClientMsg::EnsureLoopbackForward(LoopbackForwardRequest {
            pane_id,
            remote_host: remote_host.to_string(),
            remote_port,
        })
        .encode(&mut stream)?;
        match DaemonMsg::read(&mut stream)? {
            DaemonMsg::LoopbackForward(forward) => Ok(forward),
            DaemonMsg::Error(msg) => Err(anyhow::anyhow!(msg)),
            other => Err(anyhow::anyhow!(
                "unexpected reply to EnsureLoopbackForward: {other:?}"
            )),
        }
    }

    pub fn list_loopback_forwards() -> Vec<LoopbackForwardInfo> {
        fn query() -> anyhow::Result<Vec<LoopbackForwardInfo>> {
            let mut stream = connect()?;
            ClientMsg::ListLoopbackForwards.encode(&mut stream)?;
            match DaemonMsg::read(&mut stream)? {
                DaemonMsg::LoopbackForwardList(list) => Ok(list),
                other => Err(anyhow::anyhow!(
                    "unexpected reply to ListLoopbackForwards: {other:?}"
                )),
            }
        }
        query().unwrap_or_default()
    }

    pub fn close_loopback_forward(id: LoopbackForwardId) -> Vec<LoopbackForwardInfo> {
        fn query(id: LoopbackForwardId) -> anyhow::Result<Vec<LoopbackForwardInfo>> {
            let mut stream = connect()?;
            ClientMsg::CloseLoopbackForward(id).encode(&mut stream)?;
            match DaemonMsg::read(&mut stream)? {
                DaemonMsg::LoopbackForwardList(list) => Ok(list),
                other => Err(anyhow::anyhow!(
                    "unexpected reply to CloseLoopbackForward: {other:?}"
                )),
            }
        }
        query(id).unwrap_or_default()
    }

    pub fn spawn_native_ssh(
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cwd: Option<PathBuf>,
        spec: Box<NativeSshSpec>,
    ) -> anyhow::Result<(Self, u64)> {
        match Self::spawn_native_ssh_once(size, cell_w, cell_h, cwd.clone(), spec.clone()) {
            Err(first_err) if daemon_disconnected_before_spawn_reply(&first_err) => {
                if let Err(restart_err) = crate::daemon::spawn::restart() {
                    return Err(anyhow::anyhow!(
                        "daemon disconnected before SpawnNativeSsh reply ({first_err}); restart failed: {restart_err}"
                    ));
                }
                Self::spawn_native_ssh_once(size, cell_w, cell_h, cwd, spec).map_err(|second_err| {
                    anyhow::anyhow!(
                        "daemon disconnected before SpawnNativeSsh reply ({first_err}); restarted daemon but it still failed: {second_err}"
                    )
                })
            }
            other => other,
        }
    }

    fn spawn_native_ssh_once(
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cwd: Option<PathBuf>,
        spec: Box<NativeSshSpec>,
    ) -> anyhow::Result<(Self, u64)> {
        let mut stream = connect()?;
        let win = win_size(size, cell_w, cell_h);
        let endpoint = (spec.host.clone(), spec.port);
        let user = spec.user.clone();
        let auto_supplied_password = spec.password.is_some();

        ClientMsg::SpawnNativeSsh {
            cwd,
            size: win,
            spec,
        }
        .encode(&mut stream)?;
        let pane_id = match DaemonMsg::read(&mut stream)? {
            DaemonMsg::Spawned { pane_id } => pane_id,
            DaemonMsg::Error(msg) => {
                return Err(anyhow::anyhow!("daemon refused SpawnNativeSsh: {msg}"));
            }
            other => {
                return Err(anyhow::anyhow!(
                    "unexpected daemon reply to SpawnNativeSsh: {other:?}"
                ));
            }
        };

        let mut term = Self::from_stream(stream, size)?;
        term.ssh_endpoint = Some(endpoint);
        term.ssh_user = Some(user);
        term.auto_supplied_password = auto_supplied_password;
        Ok((term, pane_id))
    }

    pub fn take_auth_prompt(&self) -> Option<(u64, AuthPromptKind)> {
        self.auth_prompts
            .lock()
            .ok()
            .and_then(|mut q| q.pop_front())
    }

    pub fn take_auth_banner(&self) -> Option<String> {
        let mut q = self.auth_prompts.lock().ok()?;
        if matches!(q.front(), Some((_, AuthPromptKind::Banner { .. }))) {
            if let Some((_, AuthPromptKind::Banner { text })) = q.pop_front() {
                return Some(text);
            }
        }
        None
    }

    pub fn has_pending_auth(&self) -> bool {
        self.auth_prompts
            .lock()
            .map(|q| !q.is_empty())
            .unwrap_or(false)
    }

    pub fn ssh_phase(&self) -> Option<SshPhase> {
        self.ssh_phase.lock().ok().and_then(|g| g.clone())
    }

    pub fn ssh_endpoint(&self) -> Option<(String, u16)> {
        self.ssh_endpoint.clone()
    }

    pub fn ssh_user(&self) -> Option<String> {
        self.ssh_user.clone()
    }

    pub fn auto_supplied_password(&self) -> bool {
        self.auto_supplied_password
    }

    pub fn respond_auth(&self, request_id: u64, response: AuthResponse) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = ClientMsg::AuthResponse {
                request_id,
                response,
            }
            .encode(&mut *writer);
        }
    }

    pub fn list_known_hosts() -> Vec<KnownHostEntry> {
        fn query() -> anyhow::Result<Vec<KnownHostEntry>> {
            let mut stream = connect()?;
            ClientMsg::ListKnownHosts.encode(&mut stream)?;
            match DaemonMsg::read(&mut stream)? {
                DaemonMsg::KnownHostsList(list) => Ok(list),
                other => Err(anyhow::anyhow!(
                    "unexpected reply to ListKnownHosts: {other:?}"
                )),
            }
        }
        query().unwrap_or_default()
    }

    pub fn delete_known_host(id: KnownHostId) -> Vec<KnownHostEntry> {
        fn query(id: KnownHostId) -> anyhow::Result<Vec<KnownHostEntry>> {
            let mut stream = connect()?;
            ClientMsg::DeleteKnownHost(id).encode(&mut stream)?;
            match DaemonMsg::read(&mut stream)? {
                DaemonMsg::KnownHostsList(list) => Ok(list),
                other => Err(anyhow::anyhow!(
                    "unexpected reply to DeleteKnownHost: {other:?}"
                )),
            }
        }
        query(id).unwrap_or_default()
    }

    pub fn sftp_list(pane_id: u64, path: &str) -> Result<Vec<SftpEntry>, String> {
        fn query(pane_id: u64, path: String) -> anyhow::Result<Result<Vec<SftpEntry>, String>> {
            let mut stream = connect()?;
            ClientMsg::SftpList { pane_id, path }.encode(&mut stream)?;
            Ok(match DaemonMsg::read(&mut stream)? {
                DaemonMsg::SftpEntries(entries) => Ok(entries),
                DaemonMsg::Error(msg) => Err(msg),
                other => Err(format!("unexpected reply to SftpList: {other:?}")),
            })
        }
        query(pane_id, path.to_string()).unwrap_or_else(|e| Err(e.to_string()))
    }

    pub fn sftp_op(pane_id: u64, op: SftpOp) -> SftpOpResult {
        fn query(pane_id: u64, op: SftpOp) -> anyhow::Result<SftpOpResult> {
            let mut stream = connect()?;
            ClientMsg::SftpOp { pane_id, op }.encode(&mut stream)?;
            Ok(match DaemonMsg::read(&mut stream)? {
                DaemonMsg::SftpOpResult(result) => result,
                DaemonMsg::Error(msg) => SftpOpResult::Error(msg),
                other => SftpOpResult::Error(format!("unexpected reply to SftpOp: {other:?}")),
            })
        }
        query(pane_id, op).unwrap_or_else(|e| SftpOpResult::Error(e.to_string()))
    }

    pub fn sftp_transfer_start(spec: SftpTransferSpec) -> Result<u64, String> {
        fn query(spec: SftpTransferSpec) -> anyhow::Result<Result<u64, String>> {
            let mut stream = connect()?;
            ClientMsg::SftpTransferStart(spec).encode(&mut stream)?;
            Ok(match DaemonMsg::read(&mut stream)? {
                DaemonMsg::SftpTransferStarted { job_id } => Ok(job_id),
                DaemonMsg::Error(msg) => Err(msg),
                other => Err(format!("unexpected reply to SftpTransferStart: {other:?}")),
            })
        }
        query(spec).unwrap_or_else(|e| Err(e.to_string()))
    }

    pub fn sftp_transfer_cancel(job_id: u64) -> Vec<SftpJobProgress> {
        fn query(job_id: u64) -> anyhow::Result<Vec<SftpJobProgress>> {
            let mut stream = connect()?;
            ClientMsg::SftpTransferCancel { job_id }.encode(&mut stream)?;
            match DaemonMsg::read(&mut stream)? {
                DaemonMsg::SftpTransferProgress(jobs) => Ok(jobs),
                other => Err(anyhow::anyhow!(
                    "unexpected reply to SftpTransferCancel: {other:?}"
                )),
            }
        }
        query(job_id).unwrap_or_default()
    }

    /// A failed poll is not an empty transfer list: the caller has to be able
    /// to keep the jobs it already knows about, so this reports the failure
    /// the way `sftp_list` does rather than answering with an empty `Vec`.
    pub fn sftp_transfer_list(pane_id: u64) -> Result<Vec<SftpJobProgress>, String> {
        fn query(pane_id: u64) -> anyhow::Result<Result<Vec<SftpJobProgress>, String>> {
            let mut stream = connect()?;
            ClientMsg::SftpTransferList { pane_id }.encode(&mut stream)?;
            Ok(match DaemonMsg::read(&mut stream)? {
                DaemonMsg::SftpTransferProgress(jobs) => Ok(jobs),
                DaemonMsg::Error(msg) => Err(msg),
                other => Err(format!("unexpected reply to SftpTransferList: {other:?}")),
            })
        }
        query(pane_id).unwrap_or_else(|e| Err(e.to_string()))
    }

    /// `None` when the request never got a list back — which is not the same
    /// as getting an empty one, because only the caller of a *failed* request
    /// still has to keep showing what it had.
    pub fn add_forward(pane_id: u64, rule: SshForwardRule) -> Option<Vec<ManagedForward>> {
        fn query(pane_id: u64, rule: SshForwardRule) -> anyhow::Result<Vec<ManagedForward>> {
            let mut stream = connect()?;
            ClientMsg::AddForward { pane_id, rule }.encode(&mut stream)?;
            match DaemonMsg::read(&mut stream)? {
                DaemonMsg::ForwardList(list) => Ok(list),
                DaemonMsg::Error(msg) => Err(anyhow::anyhow!(msg)),
                other => Err(anyhow::anyhow!("unexpected reply to AddForward: {other:?}")),
            }
        }
        query(pane_id, rule)
            .inspect_err(|e| log::warn!("AddForward failed: {e}"))
            .ok()
    }

    /// `None` when the request never got a list back — see `add_forward`.
    pub fn remove_forward(pane_id: u64, forward_id: u64) -> Option<Vec<ManagedForward>> {
        fn query(pane_id: u64, forward_id: u64) -> anyhow::Result<Vec<ManagedForward>> {
            let mut stream = connect()?;
            ClientMsg::RemoveForward {
                pane_id,
                forward_id,
            }
            .encode(&mut stream)?;
            match DaemonMsg::read(&mut stream)? {
                DaemonMsg::ForwardList(list) => Ok(list),
                other => Err(anyhow::anyhow!(
                    "unexpected reply to RemoveForward: {other:?}"
                )),
            }
        }
        query(pane_id, forward_id)
            .inspect_err(|e| log::warn!("RemoveForward failed: {e}"))
            .ok()
    }

    pub fn list_forwards(pane_id: u64) -> Vec<ManagedForward> {
        fn query(pane_id: u64) -> anyhow::Result<Vec<ManagedForward>> {
            let mut stream = connect()?;
            ClientMsg::ListForwards { pane_id }.encode(&mut stream)?;
            match DaemonMsg::read(&mut stream)? {
                DaemonMsg::ForwardList(list) => Ok(list),
                other => Err(anyhow::anyhow!(
                    "unexpected reply to ListForwards: {other:?}"
                )),
            }
        }
        query(pane_id).unwrap_or_default()
    }

    const WORKSPACE_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    pub fn on_workspace(req: WorkspaceRequest) -> anyhow::Result<DaemonMsg> {
        let mut stream = connect()?;
        let _ = stream.set_read_timeout(Some(Self::WORKSPACE_OP_TIMEOUT));
        ClientMsg::OnWorkspace(Box::new(req)).encode(&mut stream)?;
        match DaemonMsg::read(&mut stream)? {
            DaemonMsg::Error(msg) => Err(anyhow::anyhow!(msg)),
            reply => Ok(reply),
        }
    }

    pub fn on_workspace_forwards(req: WorkspaceRequest) -> Vec<ManagedForward> {
        match Self::on_workspace(req) {
            Ok(DaemonMsg::ForwardList(list)) => list,
            Ok(other) => {
                log::warn!("unexpected reply to a workspace forward request: {other:?}");
                Vec::new()
            }
            Err(e) => {
                log::warn!("workspace forward request failed: {e}");
                Vec::new()
            }
        }
    }

    pub fn workspace_request(
        ws: &PaneWorkspace,
        view_pane: u64,
        op: WorkspaceOp,
    ) -> Option<WorkspaceRequest> {
        Some(WorkspaceRequest {
            workspace: ws.workspace,
            spec: ws.spec.clone()?,
            view_pane,
            op,
        })
    }

    pub fn query_procs(pane_id: u64) -> PaneProcs {
        fn query(pane_id: u64) -> anyhow::Result<PaneProcs> {
            let mut stream = connect()?;
            ClientMsg::QueryProcs { pane_id }.encode(&mut stream)?;
            match DaemonMsg::read(&mut stream)? {
                DaemonMsg::Procs(procs) => Ok(procs),
                other => Err(anyhow::anyhow!("unexpected reply to QueryProcs: {other:?}")),
            }
        }
        query(pane_id).unwrap_or_default()
    }
}

fn daemon_not_listening(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            )
        })
    })
}

fn attach_reply_wait(route: &PaneRoute) -> std::time::Duration {
    match route.is_local() {
        true => std::time::Duration::from_secs(2),
        false => std::time::Duration::from_secs(15),
    }
}

fn attach_reply_prefix(
    stream: &mut Stream,
    pane_id: u64,
    wait: std::time::Duration,
) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;

    let _ = stream.set_read_timeout(Some(wait));
    let mut buffered: Vec<u8> = Vec::new();
    let mut scratch = [0u8; 4096];
    let mut kind = None;
    while kind.is_none() {
        match stream.read(&mut scratch) {
            Ok(0) => {
                let _ = stream.set_read_timeout(None);
                return Err(anyhow::anyhow!(
                    "the daemon closed the connection without answering Attach for pane {pane_id}"
                ));
            }
            Ok(n) => buffered.extend_from_slice(&scratch[..n]),
            Err(e) if would_block(&e) => break,
            Err(e) => {
                let _ = stream.set_read_timeout(None);
                return Err(anyhow::Error::new(e).context(format!(
                    "reading the daemon's answer to Attach for pane {pane_id}"
                )));
            }
        }
        kind = crate::daemon::protocol::peek_frame_kind(&buffered);
    }
    let _ = stream.set_read_timeout(None);
    if !kind.is_some_and(crate::daemon::protocol::is_error_kind) {
        return Ok(buffered);
    }
    let message = read_error_frame(stream, &mut buffered, wait)
        .unwrap_or_else(|| format!("no such pane {pane_id}"));
    Err(anyhow::anyhow!("daemon refused Attach: {message}"))
}

fn read_error_frame(
    stream: &mut Stream,
    buffered: &mut Vec<u8>,
    wait: std::time::Duration,
) -> Option<String> {
    use std::io::Read as _;

    let _ = stream.set_read_timeout(Some(wait));
    let mut scratch = [0u8; 1024];
    let message = loop {
        match crate::daemon::protocol::take_frame(buffered) {
            Ok(Some(frame)) => match DaemonMsg::from_frame(frame.0, frame.1) {
                Ok(DaemonMsg::Error(message)) => break Some(message),
                _ => break None,
            },
            Ok(None) => match stream.read(&mut scratch) {
                Ok(0) => break None,
                Ok(n) => buffered.extend_from_slice(&scratch[..n]),
                Err(_) => break None,
            },
            Err(_) => break None,
        }
    };
    let _ = stream.set_read_timeout(None);
    message
}

fn would_block(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn daemon_disconnected_before_spawn_reply(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
            )
        })
    })
}

impl Drop for RemoteTerminal {
    fn drop(&mut self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = ClientMsg::Detach.encode(&mut *writer);
        }
        self.stop_reader();
    }
}

fn stale_mode_resets(mode: TermMode) -> Vec<u8> {
    let mut seq = Vec::new();
    if mode.contains(TermMode::ALT_SCREEN) {
        seq.extend_from_slice(b"\x1b[?1049l");
    }
    if !mode.contains(TermMode::SHOW_CURSOR) {
        seq.extend_from_slice(b"\x1b[?25h");
    }
    if mode.intersects(TermMode::MOUSE_MODE) {
        seq.extend_from_slice(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l");
    }
    if mode.contains(TermMode::SGR_MOUSE) {
        seq.extend_from_slice(b"\x1b[?1006l");
    }
    if mode.contains(TermMode::UTF8_MOUSE) {
        seq.extend_from_slice(b"\x1b[?1005l");
    }
    if mode.contains(TermMode::FOCUS_IN_OUT) {
        seq.extend_from_slice(b"\x1b[?1004l");
    }
    if mode.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL) || mode.contains(TermMode::ALT_SCREEN) {
        seq.extend_from_slice(b"\x1b[=0;1u");
    }
    seq
}

pub(crate) fn notify_desktop(title: Option<&str>, body: &str) {
    notify_desktop_for_pane(title, body, None);
}

/// Show a desktop notification.
///
/// Naming a `pane` makes the notification clickable where the platform supports
/// it, so activating it reveals that pane. It is an `EntityId` on purpose: the
/// tray dispatch identifies leaves by their gpui entity id, and taking a bare
/// `u64` here is what lets a caller hand over a `pane_id` — a different number,
/// assigned by the daemon — and get a notification that reveals nothing.
///
/// Every other case (no pane, unsupported platform, no room left to wait for a
/// click) falls back to the plain `notify-rust` path below, which is why
/// `try_clickable_notification` reports whether it took the job.
pub(crate) fn notify_desktop_for_pane(title: Option<&str>, body: &str, pane: Option<EntityId>) {
    let summary = sanitize_notification_text(title.unwrap_or("tty7"), NOTIFY_TITLE_MAX);
    let body = sanitize_notification_text(body, NOTIFY_BODY_MAX);

    if let Some(pane) = pane
        && try_clickable_notification(&summary, &body, pane.as_u64())
    {
        return;
    }

    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        ensure_notification_app();
        let mut notif = notify_rust::Notification::new();
        notif.summary(&summary).body(&body);
        // Without our own AUMID, the Windows backend falls back to
        // PowerShell's — icon and name included. Only set ours once the shell
        // has indexed a shortcut carrying it: for an AUMID it does not know,
        // `show()` reports success and drops the toast, so the ugly fallback
        // beats the branded one every time we are not sure.
        #[cfg(target_os = "windows")]
        if let Some(app_id) = crate::core::aumid::toast_app_id() {
            notif.app_id(app_id);
        }
        let _ = notif.show();
    });
}

/// Longest title / body we hand to a notification backend.
///
/// The text comes straight off the terminal (OSC 9/777 payloads, window
/// titles, an agent's own status line), so it is attacker-shaped: arbitrarily
/// long, and free to contain control bytes. Every backend truncates in its own
/// ugly way, and on Windows a stray `ESC` makes the toast XML fail to parse and
/// the whole notification disappear — so clamp both here, once, for all paths.
const NOTIFY_TITLE_MAX: usize = 96;
const NOTIFY_BODY_MAX: usize = 512;

fn sanitize_notification_text(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut taken = 0usize;
    for ch in s.chars() {
        // XML 1.0 allows exactly tab / LF / CR out of the control range.
        if ch.is_control() && !matches!(ch, '\t' | '\n' | '\r') {
            continue;
        }
        if taken == max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
        taken += 1;
    }
    out
}

/// Deliver a click-to-reveal notification, reporting whether it was taken.
/// `false` means the caller should fall back to the plain notification path.
#[cfg(all(target_os = "macos", not(test)))]
fn try_clickable_notification(title: &str, body: &str, leaf_id: u64) -> bool {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // `mac-notification-sys` blocks the calling thread until the user acts on
    // the notification, and its wait has no timeout: a banner nobody touches —
    // the common case, since unclicked ones just pile up in Notification
    // Center — parks its thread for the rest of the session. Cap how many can
    // be outstanding and let the rest through as fire-and-forget, so a chatty
    // agent cannot turn a session's notifications into a thread leak.
    const MAX_PENDING_CLICKS: usize = 8;
    static PENDING: AtomicUsize = AtomicUsize::new(0);

    if PENDING
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_PENDING_CLICKS).then_some(n + 1)
        })
        .is_err()
    {
        return false;
    }

    let (title, body) = (title.to_string(), body.to_string());
    std::thread::spawn(move || {
        show_macos_toast(&title, &body, leaf_id);
        PENDING.fetch_sub(1, Ordering::AcqRel);
    });
    true
}

#[cfg(all(target_os = "macos", not(test)))]
fn show_macos_toast(title: &str, body: &str, leaf_id: u64) {
    use mac_notification_sys::{Notification, NotificationResponse};

    // `send` sets the delivering application on first use and keeps it under a
    // `Once`, so whoever notifies first decides the name and icon for the whole
    // session. Without this the default wins — `com.apple.Finder` — and every
    // later notification, this path or `notify-rust`'s, claims to be Finder.
    ensure_notification_app();

    let response = Notification::new()
        .title(title)
        .message(body)
        .wait_for_click(true)
        .send();

    match response {
        Ok(NotificationResponse::Click) => reveal_pane(leaf_id),
        Ok(_) => {}
        Err(e) => log::warn!("failed to show macOS notification: {e}"),
    }
}

#[cfg(all(target_os = "windows", not(test)))]
fn try_clickable_notification(title: &str, body: &str, leaf_id: u64) -> bool {
    // An AUMID the shell has not indexed makes `Show` report success and drops
    // the toast on the floor, so without one the notify-rust path — which
    // deliberately keeps PowerShell's identity rather than lose the toast — is
    // the only one that shows anything at all.
    if crate::core::aumid::toast_app_id().is_none() {
        return false;
    }
    let Some(tx) = toast_thread() else {
        return false;
    };
    tx.try_send((title.to_string(), body.to_string(), leaf_id))
        .is_ok()
}

#[cfg(all(target_os = "windows", not(test)))]
type ToastRequest = (String, String, u64);

/// Toasts are built, shown and *kept* on one dedicated thread.
///
/// A `ToastNotification` has to stay alive for its `Activated` event to reach
/// us, and Windows parks toasts in Action Center long after they leave the
/// screen — a thread per toast sleeping out a fixed window would both leak
/// threads and stop working the moment the timer expired. Keeping them on a
/// single thread also means the WinRT objects never cross a thread boundary.
#[cfg(all(target_os = "windows", not(test)))]
fn toast_thread() -> Option<&'static std::sync::mpsc::SyncSender<ToastRequest>> {
    use std::sync::OnceLock;
    use std::sync::mpsc::sync_channel;

    static TX: OnceLock<Option<std::sync::mpsc::SyncSender<ToastRequest>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = sync_channel::<ToastRequest>(32);
        std::thread::Builder::new()
            .name("tty7-toasts".into())
            .spawn(move || toast_loop(rx))
            .map_err(|e| log::warn!("failed to start the toast thread: {e}"))
            .ok()
            .map(|_| tx)
    })
    .as_ref()
}

#[cfg(all(target_os = "windows", not(test)))]
fn toast_loop(rx: std::sync::mpsc::Receiver<ToastRequest>) {
    use std::collections::VecDeque;
    use windows::UI::Notifications::ToastNotification;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    // MTA lets the Activated callback run on a thread-pool thread without a
    // message loop of our own.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    // How many past toasts stay clickable. Dropping the oldest is what bounds
    // the memory here; there is no useful deadline to expire them on.
    const MAX_LIVE_TOASTS: usize = 32;
    let mut live: VecDeque<ToastNotification> = VecDeque::new();

    while let Ok((title, body, leaf_id)) = rx.recv() {
        if let Some(toast) = show_windows_toast(&title, &body, leaf_id) {
            if live.len() == MAX_LIVE_TOASTS {
                live.pop_front();
            }
            live.push_back(toast);
        }
    }
}

#[cfg(all(target_os = "windows", not(test)))]
fn show_windows_toast(
    title: &str,
    body: &str,
    leaf_id: u64,
) -> Option<windows::UI::Notifications::ToastNotification> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::core::IInspectable;

    let app_id = crate::core::aumid::toast_app_id()?;

    let xml = format!(
        r#"<toast>
  <visual>
    <binding template="ToastText02">
      <text id="1">{}</text>
      <text id="2">{}</text>
    </binding>
  </visual>
</toast>"#,
        xml_escape(title),
        xml_escape(body)
    );

    let doc = match XmlDocument::new() {
        Ok(d) => {
            if let Err(e) = d.LoadXml(&xml.into()) {
                log::warn!("failed to load toast xml: {e}");
                return None;
            }
            d
        }
        Err(e) => {
            log::warn!("failed to create toast xml document: {e}");
            return None;
        }
    };

    let toast = match ToastNotification::CreateToastNotification(&doc) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("failed to create toast notification: {e}");
            return None;
        }
    };

    if let Err(e) = toast.Activated(&TypedEventHandler::new(
        move |_sender: &Option<ToastNotification>, _args: &Option<IInspectable>| {
            reveal_pane(leaf_id);
            Ok(())
        },
    )) {
        log::warn!("failed to register toast activated handler: {e}");
    }

    let notifier = match ToastNotificationManager::CreateToastNotifierWithId(&app_id.into()) {
        Ok(n) => n,
        Err(e) => {
            log::warn!("failed to create toast notifier: {e}");
            return None;
        }
    };

    if let Err(e) = notifier.Show(&toast) {
        log::warn!("failed to show toast: {e}");
        return None;
    }

    Some(toast)
}

/// Ask the tray dispatch loop to bring `leaf_id` to the front. Runs on whatever
/// thread the platform hands the activation to, so it only touches the channel.
#[cfg(all(not(test), any(target_os = "macos", target_os = "windows")))]
fn reveal_pane(leaf_id: u64) {
    if let Some(tx) = crate::ui::tray::sender() {
        let _ = tx.try_send(crate::ui::tray::TrayAction::RevealPane { leaf_id });
    }
}

#[cfg(not(any(
    all(target_os = "macos", not(test)),
    all(target_os = "windows", not(test))
)))]
fn try_clickable_notification(_title: &str, _body: &str, _leaf_id: u64) -> bool {
    // Linux notifications go through notify-rust; click-to-reveal would need a
    // D-Bus action listener of its own.
    false
}

#[cfg(target_os = "windows")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod notification_tests {
    use super::*;

    #[test]
    fn sanitizing_drops_control_bytes_but_keeps_line_breaks() {
        let raw = "build \x1b[31mfailed\x07\nsee log\ttail";
        assert_eq!(
            sanitize_notification_text(raw, NOTIFY_BODY_MAX),
            "build [31mfailed\nsee log\ttail"
        );
    }

    #[test]
    fn sanitizing_clamps_by_chars_not_bytes() {
        let out = sanitize_notification_text(&"账".repeat(200), 8);
        assert_eq!(out, format!("{}…", "账".repeat(8)));
        assert_eq!(sanitize_notification_text("short", 8), "short");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn xml_escaping_covers_every_entity_once() {
        assert_eq!(
            xml_escape(r#"a & b < c > "d" 'e'"#),
            "a &amp; b &lt; c &gt; &quot;d&quot; &apos;e&apos;"
        );
    }
}

#[cfg(target_os = "macos")]
fn ensure_notification_app() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if notify_rust::set_application("com.github.tty7").is_err() {
            let _ = notify_rust::set_application("com.apple.Terminal");
        }
    });
}

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
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<(Option<String>, String)>) {
        self.tok.feed(bytes, |payload| {
            if let Some(note) = parse_osc_notification(payload) {
                out.push(note);
            }
        });
    }
}

fn parse_osc_notification(payload: &[u8]) -> Option<(Option<String>, String)> {
    if crate::core::cli_agent::parse_agent_event(payload).is_some() {
        return None;
    }
    let (title, body) = crate::core::osc::parse_notification(payload)?;
    if title.as_deref() == Some(crate::core::cli_agent::AGENT_EVENT_SENTINEL) {
        return None;
    }
    Some((title, body))
}

fn connect() -> anyhow::Result<Stream> {
    transport::connect().map_err(|e| {
        anyhow::Error::new(e).context(format!(
            "connect to daemon at {}",
            transport::endpoint_display()
        ))
    })
}

fn connect_routed(route: &PaneRoute) -> anyhow::Result<Stream> {
    if let PaneRoute::Unroutable(reason) = route {
        return Err(anyhow::anyhow!("{reason}"));
    }
    let Some(header) = route.header() else {
        return connect();
    };

    tty7_core::host::guard_off_ui();

    // No `ensure_wsl_server` here on purpose. The daemon runs exactly the same
    // probe inside `router::open_link` before it opens the link, so asking from
    // this side too bought nothing and cost a second full round of `wsl.exe`
    // invocations — five of them, serially, on every single pane. On a machine
    // where a `wsl.exe` round trip is slow (issue #454 measured 3.3s) that
    // duplicate was half of the wait before a new tab could take a key.
    //
    // Nothing is lost by dropping it: the returned path was discarded, the
    // failure is reported just as well through the route ack below, and the
    // first-install consent question still reaches this process — the daemon
    // runs its probe under `RouteSetup::blocking`, which installs the relay
    // that turns the question into a frame on this very connection.
    let mut stream = connect()?;
    let ack = crate::daemon::router::negotiate(&mut stream, header)
        .map_err(|e| anyhow::anyhow!("route this pane to {}: {e}", header.describe()))?;
    log::debug!(
        "pane routed to {} over {}",
        header.describe(),
        ack.link.as_deref().unwrap_or("?")
    );
    Ok(stream)
}

fn terminal_config_from_user(user_config: &crate::core::config::Config) -> Config {
    Config {
        scrolling_history: user_config.scrollback_limit,
        default_cursor_style: alacritty_cursor_style(user_config.cursor_style),
        semantic_escape_chars: user_config.word_separators.clone(),
        kitty_keyboard: true,
        // Every pane on Windows is presented through ConPTY (a shell, wsl.exe,
        // ssh.exe — conhost mediates them all), which repaints nothing after a
        // resize and keeps addressing the screen against its own re-anchored
        // layout: grow keeps rows/cursor pinned and opens blank rows below,
        // shrink scrolls the last written row to the new bottom. The grid must
        // resize the same way or every later absolute-CUP paint (PSReadLine
        // redraws its prompt that way per keystroke) lands rows off, shredding
        // the screen the first time a maximized pane draws anything.
        conpty_resize: cfg!(windows),
        ..Config::default()
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    /// The whole conhost-semantics fix hangs on this one field: it defaults to
    /// off in `alacritty_terminal`, so dropping the line compiles clean, passes
    /// every other test, and quietly resurrects the maximize garbling — which
    /// is exactly how it shipped disabled once (#415 follow-up).
    #[test]
    fn windows_panes_opt_into_conpty_resize_semantics() {
        let config = terminal_config_from_user(&crate::core::config::Config::default());
        assert_eq!(config.conpty_resize, cfg!(windows));
        #[cfg(windows)]
        assert!(config.conpty_resize);
    }
}

fn alacritty_cursor_style(style: ConfigCursorStyle) -> CursorStyle {
    let shape = match style {
        ConfigCursorStyle::Block => CursorShape::Block,
        ConfigCursorStyle::Bar => CursorShape::Beam,
        ConfigCursorStyle::Underline => CursorShape::Underline,
    };
    CursorStyle {
        shape,
        blinking: false,
    }
}

fn win_size(size: TermSize, cell_w: u16, cell_h: u16) -> WinSize {
    WinSize {
        cols: size.cols as u16,
        rows: size.rows as u16,
        cell_w,
        cell_h,
    }
}

#[cfg(all(test, windows))]
mod windows_teardown_tests {
    use super::*;

    fn tcp_pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client_side = std::net::TcpStream::connect(addr).unwrap();
        let (daemon_side, _) = listener.accept().unwrap();
        (client_side, daemon_side)
    }

    /// On Windows, `shutdown()` does not wake a thread parked in a blocking
    /// `read` on the same socket (it does on unix). `detach_link`,
    /// `adopt_relink`, and `Drop` all shut the writer down and then `join()`
    /// the reader thread, counting on that wake-up — so when the peer stays
    /// silent (a routed pane whose SSH leg went zombie: nothing arrives, no
    /// FIN ever comes), the join blocks its caller, which in production is
    /// the UI thread. This is the whole-window "not responding" hang right
    /// after a remote workspace reconnects.
    #[test]
    fn detach_link_returns_promptly_when_the_peer_stays_silent() {
        crate::core::config::pin_test_config_dir();
        let (client_side, daemon_side) = tcp_pair();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        // Let the reader thread park in read() before tearing down.
        std::thread::sleep(std::time::Duration::from_millis(200));

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            term.detach_link();
            let _ = tx.send(());
        });
        let outcome = rx.recv_timeout(std::time::Duration::from_secs(3));
        drop(daemon_side);
        assert!(
            outcome.is_ok(),
            "detach_link blocked for over 3s against a silent peer: shutdown() \
             did not unblock the reader, so join() hangs the calling thread"
        );
    }

    /// The reconnect path: `relink_panes` calls this on the UI thread right
    /// after a remote workspace re-attaches, with the old link still parked
    /// on a zombie route. It must swap links promptly, and the adopted link
    /// must actually work.
    #[test]
    fn adopt_relink_swaps_links_promptly_when_the_old_peer_stays_silent() {
        crate::core::config::pin_test_config_dir();
        let (old_client, mut _old_daemon) = tcp_pair();
        let (new_client, mut new_daemon) = tcp_pair();
        let term = RemoteTerminal::from_stream(old_client, TermSize::new(80, 24)).unwrap();
        // Let the old reader park in read() on the silent link.
        std::thread::sleep(std::time::Duration::from_millis(200));

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut term = term;
            term.adopt_relink(new_client, &PaneRoute::Local, TermSize::new(80, 24), 8, 16)
                .unwrap();
            let _ = tx.send(term);
        });
        let term = rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("adopt_relink blocked for over 3s against a silent old peer");

        DaemonMsg::Output(b"hi".to_vec())
            .encode(&mut new_daemon)
            .unwrap();
        let mut first = ' ';
        for _ in 0..200 {
            first = term.term.lock().grid()[alacritty_terminal::index::Line(0)]
                [alacritty_terminal::index::Column(0)]
            .c;
            if first == 'h' {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(first, 'h', "output on the adopted link must reach the grid");
        assert!(
            !term.exited_flag.load(Ordering::SeqCst),
            "retiring the old reader must not mark the pane as exited"
        );

        // Late traffic on the abandoned link must be ignored wholesale: an
        // Exited frame processed by the retired reader would flip
        // `child_exited` and close the freshly adopted pane from under the
        // user. Give the retired reader a full quit-poll tick to (mis)handle
        // it before checking.
        let _ = DaemonMsg::Exited { code: Some(0) }.encode(&mut _old_daemon);
        std::thread::sleep(std::time::Duration::from_millis(700));
        assert!(
            !term.child_exited(),
            "an Exited frame on the abandoned link must not close the pane"
        );
        assert!(
            !term.exited_flag.load(Ordering::SeqCst),
            "the abandoned link must not tear the adopted pane down"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    fn ssh_workspace() -> PaneWorkspace {
        PaneWorkspace {
            workspace: crate::core::session::WorkspaceId::new(),
            target: crate::core::session::RemoteTarget::Direct {
                user: "me".into(),
                host: "build-box".into(),
                port: 22,
            },
            spec: Some(Box::new(
                serde_json::from_str(
                    r#"{"host":"build-box","port":22,"user":"me","auth_mode":"auto"}"#,
                )
                .unwrap(),
            )),
        }
    }

    #[test]
    fn a_local_pane_prefixes_nothing() {
        assert!(PaneRoute::Local.header().is_none());
        assert!(PaneRoute::for_workspace(None).header().is_none());
        assert!(matches!(PaneRoute::for_workspace(None), PaneRoute::Local));
        assert!(matches!(PaneRoute::default(), PaneRoute::Local));
    }

    #[test]
    fn a_remote_pane_routes_to_its_machine_on_the_pane_channel() {
        let route = PaneRoute::for_workspace(Some(&ssh_workspace()));
        let header = route.header().expect("a remote pane is routed");
        assert_eq!(
            header.channel,
            crate::daemon::router::RouteChannel::Pane,
            "a pane must not be sent to the control socket"
        );
        assert_eq!(header.describe(), "ssh me@build-box:22");
    }

    #[test]
    fn a_wsl_workspace_routes_by_distro() {
        let ws = PaneWorkspace {
            workspace: crate::core::session::WorkspaceId::new(),
            target: crate::core::session::RemoteTarget::Wsl {
                distro: "Ubuntu-22.04".into(),
            },
            spec: None,
        };
        let route = PaneRoute::for_workspace(Some(&ws));
        let header = route.header().expect("WSL is routed");
        assert_eq!(header.describe(), "wsl Ubuntu-22.04");
        assert_eq!(header.channel, crate::daemon::router::RouteChannel::Pane);
    }

    #[test]
    fn a_local_stdio_workspace_routes_to_a_child_process_on_the_pane_dialect() {
        let ws = PaneWorkspace {
            workspace: crate::core::session::WorkspaceId::new(),
            target: crate::core::session::RemoteTarget::LocalStdio {
                program: "/tmp/tty7-server".into(),
                args: vec!["--stdio".into()],
            },
            spec: None,
        };
        let route = PaneRoute::for_workspace(Some(&ws));
        let header = route.header().expect("a local child is routable");
        assert_eq!(header.channel, crate::daemon::router::RouteChannel::Pane);
        match &header.target {
            crate::daemon::router::RouteTarget::LocalStdio { program, args } => {
                assert_eq!(program, "/tmp/tty7-server");
                assert_eq!(args, &vec!["--stdio".to_string(), "--pane".to_string()]);
            }
            other => panic!("wrong target: {other:?}"),
        }
    }

    #[test]
    fn an_unroutable_workspace_is_not_treated_as_local() {
        let ws = PaneWorkspace {
            workspace: crate::core::session::WorkspaceId::new(),
            target: crate::core::session::RemoteTarget::Alias {
                alias: "build-box".into(),
            },
            spec: None,
        };
        let route = PaneRoute::for_workspace(Some(&ws));
        assert!(matches!(route, PaneRoute::Unroutable(_)));
        assert!(route.header().is_none(), "nothing to route to");
        let err = connect_routed(&route).expect_err("must not reach the local daemon");
        assert!(err.to_string().contains("cannot be routed"), "{err}");
    }

    #[test]
    fn a_local_spawn_carries_its_workspace_so_the_shell_gets_tty7_ws() {
        let ws = "0d4e1a54-0000-4000-8000-000000000001";

        assert_eq!(
            spawn_workspace(Some(ws), &PaneRoute::Local).as_deref(),
            Some(ws),
            "without this the pane's shell has no $TTY7_WS and every \
             workspace-scoped CLI verb in it needs an explicit address"
        );

        assert_eq!(
            spawn_workspace(None, &PaneRoute::Local),
            None,
            "a pane outside any workspace must not claim one"
        );

        assert_eq!(
            spawn_workspace(Some(ws), &PaneRoute::for_workspace(Some(&ssh_workspace()))),
            None,
            "a remote server has its own machine tree; this id names a workspace in ours"
        );
    }

    #[test]
    fn only_a_local_pane_may_restart_the_local_daemon() {
        assert!(PaneRoute::Local.is_local());
        assert!(PaneRoute::for_workspace(None).is_local());

        assert!(
            !PaneRoute::for_workspace(Some(&ssh_workspace())).is_local(),
            "a routed pane's disconnect is the remote's failure, not the local daemon's"
        );
        assert!(
            !PaneRoute::Unroutable("no ssh details".into()).is_local(),
            "nothing was ever asked of the local daemon"
        );
    }

    #[test]
    fn kitty_keyboard_negotiation_reports_the_requested_mode() {
        let config = terminal_config_from_user(&crate::core::config::Config::default());
        assert!(config.kitty_keyboard);

        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Output(b"\x1b[>7u\x1b[?u".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        let mut reply = None;
        for _ in 0..200 {
            while let Ok(event) = term.events.try_recv() {
                if let AlacEvent::PtyWrite(text) = event {
                    reply = Some(text);
                }
            }
            if reply.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert_eq!(reply.as_deref(), Some("\x1b[?7u"));
        assert!(
            term.term
                .lock()
                .mode()
                .contains(TermMode::DISAMBIGUATE_ESC_CODES)
        );
    }

    #[test]
    fn deep_keyboard_mode_pushes_leave_the_reader_alive() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        let mut payload = b"\x1b[>1u".repeat(4097);
        payload.extend_from_slice(b"\x1b[?u");
        DaemonMsg::Output(payload).encode(&mut daemon_side).unwrap();
        daemon_side.flush().unwrap();

        let mut reply = None;
        for _ in 0..200 {
            while let Ok(event) = term.events.try_recv() {
                if let AlacEvent::PtyWrite(text) = event {
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
            Some("\x1b[?1u"),
            "the reader thread must survive a deep mode-push run and still answer queries"
        );
    }

    #[test]
    fn emoji_presentation_sequences_reserve_two_columns() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Output("\u{2764}\u{FE0F}x".as_bytes().to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        let mut row = String::new();
        for _ in 0..200 {
            {
                let t = term.term.lock();
                let grid = t.grid();
                row.clear();
                for col in 0..3usize {
                    row.push(
                        grid[alacritty_terminal::index::Line(0)]
                            [alacritty_terminal::index::Column(col)]
                        .c,
                    );
                }
            }
            if row.contains('x') {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert_eq!(
            row, "\u{2764} x",
            "❤️ must hold two columns (glyph + spacer) before the next glyph"
        );
    }

    #[test]
    fn spawn_retry_only_for_daemon_disconnects() {
        let eof: anyhow::Error =
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed").into();
        assert!(daemon_disconnected_before_spawn_reply(&eof));

        let refused = anyhow::anyhow!("daemon refused Spawn: configured shell missing");
        assert!(!daemon_disconnected_before_spawn_reply(&refused));
    }

    #[test]
    fn only_a_dead_daemon_is_worth_starting_one_for() {
        let connect_failed = |kind| -> anyhow::Error {
            anyhow::Error::new(std::io::Error::new(kind, "no listener"))
                .context("connect to daemon at /tmp/tty7.sock")
        };
        assert!(daemon_not_listening(&connect_failed(
            std::io::ErrorKind::ConnectionRefused
        )));
        assert!(daemon_not_listening(&connect_failed(
            std::io::ErrorKind::NotFound
        )));

        let refused = anyhow::anyhow!("daemon refused Spawn: configured shell missing");
        assert!(!daemon_not_listening(&refused));
        let eof: anyhow::Error =
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed").into();
        assert!(!daemon_not_listening(&eof));
    }

    #[test]
    fn an_attach_to_a_missing_pane_is_an_error_not_a_disconnect() {
        let (mut client_side, mut daemon_side) = UnixStream::pair().unwrap();
        DaemonMsg::Error("no such pane 7".to_string())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        let err = attach_reply_prefix(&mut client_side, 7, attach_reply_wait(&PaneRoute::Local))
            .expect_err("a missing pane must fail");
        assert!(
            format!("{err:#}").contains("no such pane 7"),
            "the daemon's own wording is what names which pane went: {err:#}"
        );
    }

    #[test]
    fn an_attach_the_daemon_hangs_up_on_is_an_error() {
        let (mut client_side, daemon_side) = UnixStream::pair().unwrap();
        drop(daemon_side);
        assert!(
            attach_reply_prefix(&mut client_side, 7, attach_reply_wait(&PaneRoute::Local)).is_err()
        );
    }

    #[test]
    fn a_local_attach_does_not_wait_as_long_as_a_remote_one() {
        let local = attach_reply_wait(&PaneRoute::Local);
        let remote = attach_reply_wait(&PaneRoute::for_workspace(Some(&ssh_workspace())));
        assert!(local < remote, "{local:?} must be the shorter wait");
        assert!(
            local <= std::time::Duration::from_secs(2),
            "the UI thread is holding still for this"
        );
    }

    #[test]
    fn a_live_attach_hands_its_replay_bytes_to_the_reader() {
        crate::core::config::pin_test_config_dir();
        let (mut client_side, mut daemon_side) = UnixStream::pair().unwrap();
        DaemonMsg::Snapshot(b"hello".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        let buffered =
            attach_reply_prefix(&mut client_side, 7, attach_reply_wait(&PaneRoute::Local))
                .expect("a live pane attaches");
        assert!(
            !buffered.is_empty(),
            "the classification read the Snapshot frame; it must come back"
        );
        let term =
            RemoteTerminal::from_stream_with(client_side, TermSize::new(80, 24), buffered).unwrap();

        let mut got = String::new();
        for _ in 0..200 {
            {
                let t = term.term.lock();
                let grid = t.grid();
                got.clear();
                for col in 0..5usize {
                    got.push(
                        grid[alacritty_terminal::index::Line(0)]
                            [alacritty_terminal::index::Column(col)]
                        .c,
                    );
                }
            }
            if got == "hello" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            got, "hello",
            "the pre-read replay must still reach the grid"
        );
    }

    #[test]
    fn reader_feeds_local_grid() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();

        let size = TermSize::new(80, 24);
        let term = RemoteTerminal::from_stream(client_side, size).unwrap();

        DaemonMsg::Output(b"hello".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        DaemonMsg::Cwd(PathBuf::from("/tmp/work"))
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

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

        let mut cwd = None;
        for _ in 0..200 {
            cwd = term.foreground_cwd();
            if cwd.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(cwd, Some(PathBuf::from("/tmp/work")));

        drop(daemon_side);
        for _ in 0..200 {
            if term.exited_flag.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(term.exited_flag.load(Ordering::SeqCst));
    }

    #[test]
    fn cursor_style_sequence_overrides_and_resets_to_user_default() {
        use alacritty_terminal::vte::ansi::CursorShape;

        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        let mut user_config = crate::core::config::Config::default();
        user_config.cursor_style = ConfigCursorStyle::Underline;
        term.apply_user_config(&user_config);

        let mut shape = term.term.lock().cursor_style().shape;
        assert_eq!(shape, CursorShape::Underline);

        DaemonMsg::Output(b"\x1b[6 q".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        for _ in 0..200 {
            shape = term.term.lock().cursor_style().shape;
            if shape == CursorShape::Beam {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(shape, CursorShape::Beam);

        DaemonMsg::Output(b"\x1b[0 q".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        for _ in 0..200 {
            shape = term.term.lock().cursor_style().shape;
            if shape == CursorShape::Underline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(shape, CursorShape::Underline);
    }

    #[test]
    fn reader_surfaces_auth_prompt_and_status() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::SshStatus {
            phase: SshPhase::Authenticating,
        }
        .encode(&mut daemon_side)
        .unwrap();
        DaemonMsg::AuthPrompt {
            request_id: 7,
            prompt: AuthPromptKind::Password {
                user: "deploy".into(),
                host: "10.0.0.5".into(),
            },
        }
        .encode(&mut daemon_side)
        .unwrap();
        daemon_side.flush().unwrap();

        let mut prompt = None;
        for _ in 0..200 {
            if let Some(p) = term.take_auth_prompt() {
                prompt = Some(p);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (id, kind) = prompt.expect("auth prompt should have surfaced");
        assert_eq!(id, 7);
        assert!(matches!(kind, AuthPromptKind::Password { .. }));
        assert_eq!(term.ssh_phase(), Some(SshPhase::Authenticating));
        assert!(!term.has_pending_auth());
    }

    #[test]
    fn child_exit_is_distinguished_from_daemon_disconnect() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        DaemonMsg::Exited { code: Some(0) }
            .encode(&mut daemon_side)
            .unwrap();
        for _ in 0..200 {
            if term.exited_flag.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(term.exited_flag.load(Ordering::SeqCst));
        assert!(
            term.child_exited(),
            "an Exited frame is a genuine child exit"
        );

        let (client_side, daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        drop(daemon_side);
        for _ in 0..200 {
            if term.exited_flag.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(term.exited_flag.load(Ordering::SeqCst));
        assert!(
            !term.child_exited(),
            "a disconnect is not a child exit — auto-close must not fire"
        );
    }

    #[test]
    fn stale_mode_resets_target_only_the_dirty_bits() {
        let clean = TermMode::SHOW_CURSOR | TermMode::LINE_WRAP | TermMode::BRACKETED_PASTE;
        assert!(stale_mode_resets(clean).is_empty());

        let hidden = TermMode::LINE_WRAP;
        assert_eq!(stale_mode_resets(hidden), b"\x1b[?25h");

        let residue = TermMode::ALT_SCREEN | TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
        let seq = stale_mode_resets(residue);
        let text = String::from_utf8_lossy(&seq).into_owned();
        assert!(text.starts_with("\x1b[?1049l"));
        assert!(text.contains("\x1b[?25h"));
        assert!(text.contains("\x1b[?1002l"));
        assert!(text.contains("\x1b[?1006l"));
        assert!(text.ends_with("\x1b[=0;1u"));

        let kitty = TermMode::SHOW_CURSOR | TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(stale_mode_resets(kitty), b"\x1b[=0;1u");
    }

    #[test]
    fn prompt_report_scrubs_stale_tui_modes() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Output(b"\x1b[?1049h\x1b[?25l\x1b[?1002h\x1b[?1006h".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(255),
        }
        .encode(&mut daemon_side)
        .unwrap();
        daemon_side.flush().unwrap();

        let mut mode = TermMode::NONE;
        for _ in 0..200 {
            mode = *term.term.lock().mode();
            let scrubbed = !mode.contains(TermMode::ALT_SCREEN)
                && mode.contains(TermMode::SHOW_CURSOR)
                && !mode.intersects(TermMode::MOUSE_MODE)
                && !mode.contains(TermMode::SGR_MOUSE);
            if scrubbed && term.at_prompt() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !mode.contains(TermMode::ALT_SCREEN),
            "the prompt report must pull the grid off the stranded alt screen"
        );
        assert!(
            mode.contains(TermMode::SHOW_CURSOR),
            "the prompt report must re-show the DECTCEM-hidden cursor"
        );
        assert!(
            !mode.intersects(TermMode::MOUSE_MODE) && !mode.contains(TermMode::SGR_MOUSE),
            "the prompt report must disable stale mouse reporting"
        );
    }

    #[test]
    fn snapshot_replay_suppresses_query_replies_and_side_effects() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Snapshot(b"\x1b[6n\x1b]11;?\x07\x1b]52;c;aGk=\x07\x07replayed".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

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

    #[test]
    fn write_sends_input_frames() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        term.write(Vec::<u8>::new());
        term.write(b"echo hi\r".to_vec());

        match ClientMsg::read(&mut daemon_side).unwrap() {
            ClientMsg::Input(bytes) => assert_eq!(bytes, b"echo hi\r"),
            other => panic!("expected Input, got {other:?}"),
        }
    }

    #[test]
    fn attach_replay_runs_at_the_daemon_reported_size() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

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

    #[test]
    fn first_resize_always_syncs_then_dedups() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        term.resize(TermSize::new(80, 24), 8, 17);
        match ClientMsg::read(&mut daemon_side).unwrap() {
            ClientMsg::Resize(ws) => assert_eq!((ws.cols, ws.rows), (80, 24)),
            other => panic!("expected the first Resize to be sent, got {other:?}"),
        }

        term.resize(TermSize::new(80, 24), 8, 17);
        term.write(b"marker".to_vec());
        match ClientMsg::read(&mut daemon_side).unwrap() {
            ClientMsg::Input(bytes) => assert_eq!(bytes, b"marker"),
            other => panic!("expected Input (dup resize sends nothing), got {other:?}"),
        }
    }

    #[test]
    fn sync_update_without_esu_flushes_after_the_deadline() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Output(b"\x1b[?2026habc".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

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

    #[test]
    fn snapshot_replay_flushes_a_dangling_sync_frame_suppressed() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Snapshot(b"\x1b[?2026h\x1b[6nhi".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

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

        let mut events = Vec::new();
        while let Ok(ev) = term.events.try_recv() {
            events.push(ev);
        }
        assert!(
            !events.iter().any(|e| matches!(e, AlacEvent::PtyWrite(_))),
            "a query inside the replayed sync tail must stay suppressed"
        );
    }

    /// Feeds one conhost-shaped repaint and reports the cell the cursor ends on,
    /// waiting for the `X` the frame paints so the reader is known to be done.
    fn cursor_after_conpty_frame(frame: &[u8]) -> (i32, usize) {
        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // Where the TUI put the cursor before conhost repainted over it.
        DaemonMsg::Output(b"\x1b[6;4H".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        DaemonMsg::Output(frame.to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        for _ in 0..600 {
            {
                let t = term.term.lock();
                let painted = t.grid()[alacritty_terminal::index::Line(19)]
                    [alacritty_terminal::index::Column(1)]
                .c;
                if painted == 'X' {
                    let point = t.grid().cursor.point;
                    return (point.line.0, point.column.0);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the reader never applied the frame");
    }

    #[test]
    fn a_conpty_frame_that_shows_the_cursor_over_an_erase_keeps_the_cell_it_hid_on() {
        let got = cursor_after_conpty_frame(
            b"\x1b[?25l\x1b[20;2HX\x1b[K\x1b[m\x1b[22;42H\x1b[K\x1b[?25h",
        );
        if RemoteTerminal::REPAIR_PARKED_CURSOR {
            assert_eq!(
                got,
                (5, 3),
                "conhost parked the cursor on the cell it erased last; the cursor \
                 belongs where it was when the repaint hid it"
            );
        } else {
            assert_eq!(
                got,
                (21, 41),
                "with no conhost in between the stream is the application's own, \
                 and the cell it left the cursor on is the cell it meant"
            );
        }
    }

    #[test]
    fn a_conpty_frame_that_moves_the_cursor_before_showing_it_is_obeyed() {
        assert_eq!(
            cursor_after_conpty_frame(
                b"\x1b[?25l\x1b[20;2HX\x1b[K\x1b[m\x1b[22;42H\x1b[K\x1b[9;9H\x1b[?25h"
            ),
            (8, 8),
            "the frame painted the cursor somewhere on purpose"
        );
    }

    /// Issue #430. Vim opens its command line with exactly the shape the parked
    /// -cursor scanner calls parked — hide, move around to paint, end on the `:`
    /// it wrote — and then echoes every following keystroke as a bare byte at
    /// wherever that left the cursor. Putting the cursor back on a raw pty
    /// therefore does not straighten out a stray caret, it drops `wq!` onto the
    /// row vim was editing. Bytes below are a capture of vim 9 on a 20x11 pty.
    #[test]
    fn a_raw_pty_repaint_keeps_the_cursor_the_frame_left_so_the_echo_lands_on_it() {
        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(20, 11)).unwrap();

        let mut stream: Vec<u8> = Vec::new();
        // `vim test.md`: the alternate screen, the file, cursor home.
        stream.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2J\x1b[1;1H123456789\x1b[1;1H");
        // Esc, then `:` — two bracketed repaints, the second ending on the `:`
        // vim wrote at the head of the command line.
        stream.extend_from_slice(b"\x1b[?25l\x1b[m\x1b[11;10H^[\x1b[1;1H\x1b[?25h");
        stream.extend_from_slice(b"\x1b[?25l\x1b[11;10H  \x1b[1;1H\x07\x1b[?25h");
        stream.extend_from_slice(
            b"\x1b[?25l\x1b[11;10H:\x1b[1;1H\x1b[11;1H\x1b[K\x1b[11;1H:\x1b[?25h",
        );
        // `w`, `q`, `!`: vim echoes them with no positioning of their own.
        stream.extend_from_slice(b"wq!");
        DaemonMsg::Output(stream).encode(&mut daemon_side).unwrap();
        daemon_side.flush().unwrap();

        let row = |t: &Term<EventProxy>, line: i32| -> String {
            (0..20)
                .map(|col| {
                    t.grid()[alacritty_terminal::index::Line(line)]
                        [alacritty_terminal::index::Column(col)]
                    .c
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        };

        // The whole batch is applied under one lock, so the `:` landing on the
        // command line means every byte after it landed too.
        let mut command_line = String::new();
        let mut edited = String::new();
        for _ in 0..600 {
            {
                let t = term.term.lock();
                command_line = row(&t, 10);
                edited = row(&t, 0);
            }
            if command_line.starts_with(':') {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            command_line, ":wq!",
            "the keystrokes belong after the `:` the repaint ended on"
        );
        assert_eq!(
            edited, "123456789",
            "and nothing of them belongs on the row vim was editing"
        );
    }

    #[test]
    fn layout_resize_reasserts_geometry_after_a_late_size_frame() {
        use alacritty_terminal::grid::Dimensions as _;
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        term.resize(TermSize::new(100, 40), 8, 17);
        assert!(matches!(
            ClientMsg::read(&mut daemon_side).unwrap(),
            ClientMsg::Resize(_)
        ));

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

        term.resize(TermSize::new(100, 40), 8, 17);
        assert_eq!(term.term.lock().columns(), 100);
        assert_eq!(term.term.lock().screen_lines(), 40);
        assert!(matches!(
            ClientMsg::read(&mut daemon_side).unwrap(),
            ClientMsg::Resize(ws) if ws.cols == 100 && ws.rows == 40
        ));
    }

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

    #[test]
    fn echoed_resize_defers_the_grid_reflow_to_the_daemons_size_frame() {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line};

        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        term.force_resize_echo = true;

        term.resize(TermSize::new(120, 30), 8, 17);
        assert!(matches!(
            ClientMsg::read(&mut daemon_side).unwrap(),
            ClientMsg::Resize(ws) if ws.cols == 120 && ws.rows == 30
        ));
        assert_eq!(
            term.term.lock().columns(),
            80,
            "the grid keeps the old geometry until the daemon's echo arrives"
        );

        // Old-width bytes still in flight ahead of the echo: a CUP to column
        // 100 must clamp on the 80-col grid. The same addressing after the
        // echo reaches the real column on the 120-col grid.
        DaemonMsg::Output(b"\x1b[1;100HA".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        DaemonMsg::Size(WinSize {
            cols: 120,
            rows: 30,
            cell_w: 8,
            cell_h: 17,
        })
        .encode(&mut daemon_side)
        .unwrap();
        DaemonMsg::Output(b"\x1b[2;100HB".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        for _ in 0..400 {
            {
                let t = term.term.lock();
                if t.columns() == 120 && t.grid()[Line(1)][Column(99)].c == 'B' {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let t = term.term.lock();
        let grid = t.grid();
        assert_eq!(
            grid[Line(0)][Column(79)].c,
            'A',
            "bytes queued before the echo parse at the old width"
        );
        assert_eq!(
            grid[Line(1)][Column(99)].c,
            'B',
            "bytes after the echo parse at the new width"
        );
    }

    #[test]
    fn at_prompt_stays_false_while_shell_integration_is_inactive() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        assert!(!term.shell_active(), "no report yet → integration inactive");

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

    #[test]
    fn at_prompt_follows_daemon_prompt_reports() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

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

    #[test]
    fn foreground_agent_follows_daemon_agent_reports() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        assert_eq!(term.foreground_agent(), None, "none before any report");

        let poll = |want: Option<CLIAgent>| {
            for _ in 0..200 {
                if term.foreground_agent() == want {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };

        DaemonMsg::Agent(Some(CLIAgent::Claude))
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(Some(CLIAgent::Claude)), "agent report should surface");

        DaemonMsg::Agent(None).encode(&mut daemon_side).unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(None), "agent exit should clear it");
    }

    #[test]
    fn agent_session_follows_daemon_status_reports() {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus};

        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        assert_eq!(term.agent_session(), None, "none before any report");

        let poll = |want: &dyn Fn(Option<AgentSessionState>) -> bool| {
            for _ in 0..200 {
                if want(term.agent_session()) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };

        DaemonMsg::AgentStatus(Some(AgentSessionState {
            status: AgentStatus::Waiting,
            message: Some("Claude needs your permission".into()),
            session_id: Some("sid-1".into()),
            launch_argv: None,
            rich: true,
            cwd: None,
            activity: 0,
        }))
        .encode(&mut daemon_side)
        .unwrap();
        daemon_side.flush().unwrap();
        assert!(
            poll(&|s| s.is_some_and(|s| s.status == AgentStatus::Waiting
                && s.session_id.as_deref() == Some("sid-1")
                && s.rich)),
            "status report should surface with message + session id"
        );

        DaemonMsg::AgentStatus(None)
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(&|s| s.is_none()), "a None report clears the session");
    }

    #[test]
    fn the_spawn_directory_answers_until_the_shell_reports_its_own() {
        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        let poll = |want: Option<&str>| {
            let want = want.map(PathBuf::from);
            for _ in 0..200 {
                if term.foreground_cwd() == want {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };

        assert_eq!(term.foreground_cwd(), None, "nothing known before the seed");
        term.seed_cwd(Some(PathBuf::from("/repo/tty7")));
        assert_eq!(
            term.foreground_cwd(),
            Some(PathBuf::from("/repo/tty7")),
            "the sidebar can group this pane without waiting for the shell"
        );

        DaemonMsg::Cwd(PathBuf::from("/repo/tty7/crates"))
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(
            poll(Some("/repo/tty7/crates")),
            "the shell's own report replaces the seed"
        );
    }

    #[test]
    fn a_shell_report_that_beat_the_seed_wins() {
        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Cwd(PathBuf::from("/somewhere/else"))
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        for _ in 0..200 {
            if term.foreground_cwd().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        term.seed_cwd(Some(PathBuf::from("/repo/tty7")));
        assert_eq!(
            term.foreground_cwd(),
            Some(PathBuf::from("/somewhere/else")),
            "where the shell actually landed beats where we asked it to"
        );
    }

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

    #[test]
    fn the_running_command_is_the_line_the_shell_marked_and_ends_with_it() {
        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        let poll = |want: &str| {
            for _ in 0..200 {
                if term.running_command() == want {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };
        assert_eq!(term.running_command(), "", "nothing runs before a mark");

        // The escaping is the integration's; this side stores it verbatim and
        // the reader of the field undoes it.
        DaemonMsg::Output(b"\x1b]133;C;printf '100%25'\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(
            poll("printf '100%25'"),
            "C should record the line it carried, got {:?}",
            term.running_command()
        );

        // Back at a prompt the command is over — holding the name would make
        // the next close question ask about something that already finished.
        DaemonMsg::Output(b"\x1b]133;B\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(""), "a new prompt clears the running command");

        // A bare `C` starts a command without naming it (the PowerShell path).
        // It must not leave the previous name standing.
        DaemonMsg::Output(b"\x1b]133;C;cargo build\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll("cargo build"));
        DaemonMsg::Output(b"\x1b]133;C\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(""), "an unnamed command start does not inherit a name");
    }

    #[test]
    fn shell_vi_mode_follows_live_prompt_mode_marks_without_disarming_zle() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        let poll = |vi: bool, zle: bool| {
            for _ in 0..200 {
                if term.shell_vi_mode() == vi && term.zle_reading() == zle {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };

        assert!(!term.shell_vi_mode(), "conservative false before any mark");
        assert!(!term.zle_reading(), "zle also starts false");

        DaemonMsg::Output(b"\x1b]133;B\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(false, true), "B should arm zle only");

        DaemonMsg::Output(b"\x1b]133;V;1\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(
            poll(true, true),
            "V;1 should set shell vi-mode without disarming zle"
        );

        DaemonMsg::Output(b"\x1b]133;V;0\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(
            poll(false, true),
            "V;0 should clear shell vi-mode without disarming zle"
        );

        DaemonMsg::Output(b"\x1b]133;C\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(false, false), "C still disarms zle");
    }

    #[test]
    fn shell_vi_mode_is_restored_from_snapshot_replay() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        let poll = |vi: bool| {
            for _ in 0..200 {
                if term.shell_vi_mode() == vi {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };

        DaemonMsg::Snapshot(b"\x1b]133;V;1\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(
            poll(true),
            "attached clients should inherit the prompt's vi-mode state"
        );
        assert!(
            !term.zle_reading(),
            "historical replay must not imply zle is currently reading"
        );

        DaemonMsg::Snapshot(b"\x1b]133;V;0\x07".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        assert!(poll(false), "a replayed V;0 should clear vi-mode state");
    }

    fn full_dump(term: &RemoteTerminal) -> String {
        use alacritty_terminal::grid::Dimensions as _;
        let t = term.term.lock();
        let grid = t.grid();
        let mut out = String::new();
        for l in -(grid.history_size() as i32)..grid.screen_lines() as i32 {
            for c in 0..grid.columns() {
                out.push(
                    grid[alacritty_terminal::index::Line(l)][alacritty_terminal::index::Column(c)]
                        .c,
                );
            }
            out.push('\n');
        }
        out
    }

    fn tui_frame(lines: &[String], prev_rows: usize) -> Vec<u8> {
        let mut b = Vec::new();
        if prev_rows > 1 {
            b.extend_from_slice(format!("\r\x1b[{}A\x1b[J", prev_rows - 1).as_bytes());
        }
        b.extend_from_slice(lines.join("\r\n").as_bytes());
        b
    }

    #[test]
    fn segmented_ring_replay_reproduces_live_rendering() {
        const MARK: &str = "DUPMARK";
        let frame_lines = |f: usize| -> Vec<String> {
            (0..10)
                .map(|i| format!("{MARK} f{f:02} l{i:02} {:.<74}", ""))
                .collect()
        };
        let mut history = Vec::new();
        for f in 0..8 {
            history.extend(tui_frame(&frame_lines(f), if f == 0 { 0 } else { 10 }));
        }

        let wait_for = |term: &RemoteTerminal, needle: &str| -> String {
            let mut dump = String::new();
            for _ in 0..400 {
                dump = full_dump(term);
                if dump.contains(needle) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            dump
        };
        let ws = |cols: u16| WinSize {
            cols,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        };

        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let mut live = RemoteTerminal::from_stream(client_side, TermSize::new(100, 24)).unwrap();
        DaemonMsg::Output(history.clone())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        let dump = wait_for(&live, "f07 l09");
        assert!(dump.contains("f07 l09"), "live output should have landed");
        live.resize(TermSize::new(80, 24), 8, 17);
        let live_count = full_dump(&live).matches(MARK).count();
        assert_eq!(
            live_count, 10,
            "live rendering is clean: each redraw erases the previous frame, \
             so exactly one 10-line copy survives the resize"
        );

        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let replay = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        DaemonMsg::Size(ws(100)).encode(&mut daemon_side).unwrap();
        DaemonMsg::Snapshot(history.clone())
            .encode(&mut daemon_side)
            .unwrap();
        DaemonMsg::Size(ws(80)).encode(&mut daemon_side).unwrap();
        DaemonMsg::Snapshot(Vec::new())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        let dump = wait_for(&replay, "f07 l09");
        {
            use alacritty_terminal::grid::Dimensions as _;
            let mut cols = 0;
            for _ in 0..400 {
                cols = replay.term.lock().columns();
                if cols == 80 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert_eq!(cols, 80, "the trailing pair must end the grid at 80 cols");
        }
        let replay_count = dump.matches(MARK).count();
        assert_eq!(
            replay_count, live_count,
            "the segmented replay must reproduce the live rendering exactly"
        );

        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let flat = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        DaemonMsg::Size(ws(80)).encode(&mut daemon_side).unwrap();
        DaemonMsg::Snapshot(history)
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        let dump = wait_for(&flat, "f07 l09");
        let flat_count = dump.matches(MARK).count();
        assert!(
            flat_count > live_count,
            "flat replay at the final width should duplicate (got {flat_count}); \
             if it stopped, the segmented path may no longer be exercising anything"
        );
    }
}

#[cfg(test)]
mod osc_tests {
    use super::{OscNotifyScanner, parse_osc_notification};

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
        assert_eq!(
            scan(&[b"\x1b]9;Build done\x07"]),
            vec![(None, "Build done".to_string())]
        );
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
        assert_eq!(
            scan(&[b"\x1b]777;notify;Just a message\x1b\\"]),
            vec![(None, "Just a message".to_string())]
        );
    }

    #[test]
    fn split_across_reads_is_reassembled() {
        assert_eq!(
            scan(&[b"\x1b]9;Hel", b"lo wor", b"ld\x07"]),
            vec![(None, "Hello world".to_string())]
        );
        assert_eq!(
            scan(&[b"\x1b]9;Ping\x1b", b"\\"]),
            vec![(None, "Ping".to_string())]
        );
    }

    #[test]
    fn uninteresting_osc_is_ignored_cheaply() {
        assert_eq!(
            scan(&[b"\x1b]52;c;bWFueSBieXRlcw==\x07\x1b]0;my title\x07"]),
            vec![]
        );
        assert_eq!(
            scan(&[b"\x1b]0;title\x07\x1b]9;After\x07"]),
            vec![(None, "After".to_string())]
        );
    }

    #[test]
    fn conemu_osc9_subcommands_are_not_notifications() {
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

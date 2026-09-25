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

use crate::terminal::command_cursor::{CommandCursorStyle, CommandMark};
use crate::terminal::parked_cursor::{CursorCut, ParkedCursorRepair, ParkedCursorScanner};

use std::collections::VecDeque;

use crate::core::cli_agent::{AgentSessionState, AgentStatus, CLIAgent};
use crate::core::config::CursorStyle as ConfigCursorStyle;
use crate::core::osc::{OscTokenizer, TitleEffect, TitleLifetime};
use crate::daemon::protocol::{
    AuthPromptKind, AuthResponse, ClientMsg, DaemonMsg, KnownHostEntry, KnownHostId,
    LoopbackForward, LoopbackForwardRequest, ManagedForward, NativeSshSpec, PaneProcs,
    RemoteContext, RemoteKind, RestoreFrom, SftpEntry, SftpJobProgress, SftpOp, SftpOpResult,
    SftpTransferSpec, ShellSpec, SshForwardRule, SshPhase, SshTestReport, WinSize, WorkspaceOp,
    WorkspaceRequest,
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

/// The pane's agent status as the client last heard it, plus how it heard it.
///
/// A reattach — the app restarting onto panes the daemon kept alive, or a
/// dropped link coming back — has the daemon replay the pane's *stored* status
/// as an ordinary `AgentStatus` frame ([`crate::daemon`]'s `replay_state`).
/// Nothing on the wire distinguishes it from a live transition, and a client
/// that reads it as one concludes that every restored agent finished its turn
/// in the instant the window opened. `replayed` is that distinction, kept in
/// the same lock as the value it describes so a reader can never observe the
/// status without also learning where it came from.
#[derive(Default)]
struct AgentSlot {
    state: Option<AgentSessionState>,
    /// `state` arrived as an attach replay and no one has adopted it yet.
    /// Cleared by the first taker — the view adopts it as a baseline rather
    /// than as an edge.
    replayed: bool,
}

struct ReaderSignals {
    cwd: Arc<Mutex<Option<PathBuf>>>,
    shell: Arc<Mutex<ShellState>>,
    remote: Arc<Mutex<Option<RemoteContext>>>,
    agent: Arc<Mutex<Option<CLIAgent>>>,
    agent_session: Arc<Mutex<AgentSlot>>,
    /// Whether this link still owes us the attach replay. The reader keeps it
    /// as a plain local: a link's replay is a property of that link's stream
    /// position, and nothing outside the reader thread ever needs to read it.
    awaiting_replay: bool,
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
    clipboard_writes: Arc<Mutex<VecDeque<tty7_core::core::clipboard::ClipboardWrite>>>,
    clipboard_write_busy: Arc<AtomicBool>,
    /// Whether this pane's pty is one a conhost renders into, and so whether
    /// the reader puts back the cursor a repaint parked. Decided per pane from
    /// its [`PtySource`], and shared rather than copied because the reader can
    /// learn better mid-stream — see the `RemoteContext` arm.
    local_conpty: Arc<AtomicBool>,
}

/// What kind of pty is at the far end of a pane's link, which is what decides
/// whether a conhost stands between the application and us.
///
/// The distinction is not the platform this client was built for. A Windows
/// client's panes are a mix: a local shell — or `wsl.exe`, or an `ssh` client,
/// which are ordinary programs inside the same ConPTY — is rendered by conhost,
/// while a pane on a remote `tty7-server` (Linux or macOS only, see
/// [`tty7_core::daemon::install::asset::asset_for_uname`]) or on a native-SSH
/// channel is a raw unix pty whose bytes reach us untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtySource {
    /// A pty this machine's Windows daemon opened: a ConPTY, with conhost
    /// painting frames into it.
    LocalConpty,
    /// A pty nothing repaints on our behalf — every pty on unix, and every pty
    /// reached over a link.
    Raw,
}

impl PtySource {
    /// The source a pane spawned or attached on `route` reads from. The
    /// platform enters here and nowhere else: a local route means a pty this
    /// machine opened, and only on Windows is that a ConPTY.
    ///
    /// `Unroutable` is a route that could not be resolved, so it never gets a
    /// pty at all; answering as if it were local costs nothing and keeps the
    /// match total.
    fn for_route(route: &PaneRoute) -> PtySource {
        match route {
            PaneRoute::Local | PaneRoute::Unroutable(_) if cfg!(windows) => PtySource::LocalConpty,
            _ => PtySource::Raw,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaneWorkspace {
    pub workspace: crate::core::session::WorkspaceId,
    pub target: crate::core::session::RemoteTarget,
    pub spec: Option<Box<NativeSshSpec>>,
    /// The name the workspace answers to — its own label, or its machine's
    /// when it has none. A pane keeps answering to this name when its link
    /// dies (#438 gave SSH panes their host here; without this, a workspace
    /// pane fell back to the bare app name and the tab read "tty7 —
    /// disconnected").
    pub label: Option<String>,
    /// Whether the daemon serving this workspace's panes echoes a `Size` frame
    /// when it applies a resize — read off the host's control hello by whoever
    /// built this value, because this module may not ask the network itself.
    /// `false` whenever the answer is unknown (link down, server too old to
    /// advertise), which keeps the safe reflow-at-request behavior.
    pub resize_echo: bool,
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
            (_, None) => {
                // Deliberately not the target: a `Profile` spells itself as
                // its config UUID in `Display` and in `Debug` alike, and a
                // deleted profile is exactly what empties `spec` here. This
                // sentence is not only logged — `land_pane` hands it to the
                // pending pane, which prints the reason verbatim under
                // "could not reach {machine}", so the UUID reached the screen
                // (#485). The workspace's own name is what every other
                // surface calls this thing.
                return Err(match self.label.as_deref() {
                    Some(label) => anyhow::anyhow!(
                        "{label} has no SSH connection details, so its panes cannot be routed"
                    ),
                    None => anyhow::anyhow!(
                        "this workspace has no SSH connection details, so its panes \
                         cannot be routed"
                    ),
                });
            }
        };
        Ok(header.for_pane())
    }
}

#[derive(Clone, Debug, Default)]
pub enum PaneRoute {
    #[default]
    Local,
    Remote {
        header: Box<crate::daemon::router::RouteHeader>,
        /// Whether the daemon at the far end of this route echoes a `Size`
        /// frame when it applies a resize, per its host's control hello (see
        /// [`PaneWorkspace::resize_echo`]). Riding on the route puts the
        /// answer everywhere a route is assigned — spawn, attach, relink —
        /// and a relink builds its route fresh, so a reconnect re-answers it.
        resize_echo: bool,
    },
    Unroutable(String),
}

impl PaneRoute {
    pub fn for_workspace(workspace: Option<&PaneWorkspace>) -> PaneRoute {
        match workspace {
            None => PaneRoute::Local,
            Some(ws) => match ws.route_header() {
                Ok(header) => PaneRoute::Remote {
                    header: Box::new(header),
                    resize_echo: ws.resize_echo,
                },
                Err(e) => PaneRoute::Unroutable(e.to_string()),
            },
        }
    }

    pub fn header(&self) -> Option<&crate::daemon::router::RouteHeader> {
        match self {
            PaneRoute::Remote { header, .. } => Some(header),
            PaneRoute::Local | PaneRoute::Unroutable(_) => None,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, PaneRoute::Local)
    }

    fn allow_remote_clipboard_write(&self) -> bool {
        match self {
            PaneRoute::Remote { header, .. } => match &header.target {
                crate::daemon::router::RouteTarget::Ssh(spec) => spec.remote_clipboard_write,
                _ => false,
            },
            PaneRoute::Local | PaneRoute::Unroutable(_) => false,
        }
    }
}

/// How much unsent input a pane holds before it calls the link lost. A link
/// that is draining never gets near this: the sender thread is parked waiting
/// for work, so the queue holds at most the frames of one burst. Reaching it
/// means nothing on the far side has taken a byte for as long as it takes to
/// type — or paste — four megabytes, which is a dead link, not a slow one.
const MAX_BACKLOG: usize = 4 << 20;

/// How long a teardown lets the sender finish what is already queued before it
/// cuts the socket out from under it. `Detach` is the last frame a pane sends
/// and it is worth a moment: on a draining link the sender is idle, so it goes
/// out in microseconds and this returns at once. On a link that has stopped
/// draining it will never go out at all, and closing the pane must not wait
/// around to discover that — the daemon reads the closed socket as a detach
/// anyway.
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(50);

/// How long a resize the daemon promised to echo is taken to still be on its
/// way. Past this, a grid that has not reached the requested geometry is not
/// waiting on anything: the request or its echo went missing, and the size is
/// sent again. The local daemon echoes in well under a millisecond and a
/// routed one within a round trip; a resend that beats a slow echo costs one
/// `TIOCSWINSZ` of the size the pty already has, which signals nobody.
const RESIZE_ECHO_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether `term`'s grid is `size`, as the emulator would size it — it never
/// goes below `MIN_COLUMNS` × `MIN_SCREEN_LINES`, so a sliver of a pane asking
/// for one column is already as narrow as it can get.
fn grid_holds(term: &Term<EventProxy>, size: TermSize) -> bool {
    use alacritty_terminal::grid::Dimensions as _;
    use alacritty_terminal::term::{MIN_COLUMNS, MIN_SCREEN_LINES};
    term.columns() == size.cols.max(MIN_COLUMNS)
        && term.screen_lines() == size.rows.max(MIN_SCREEN_LINES)
}

/// What the sender thread needs to report a link that stopped taking input.
/// The signals the pane holds, cloned out so the failure can be raised from the
/// thread that actually meets it.
#[derive(Clone)]
struct InputLoss {
    /// Set by the first frame *this* link refused, so the loss is said once
    /// rather than once per keystroke. One per link rather than one per pane: a
    /// relink hands the retired sender's last, doomed write and the new
    /// sender's first real one two different flags, so the dying link cannot
    /// spend the new link's one chance to speak.
    said: Arc<AtomicBool>,
    reader_quit: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    proxy: EventProxy,
}

impl InputLoss {
    fn new(reader_quit: Arc<AtomicBool>, exited: Arc<AtomicBool>, proxy: EventProxy) -> InputLoss {
        InputLoss {
            said: Arc::new(AtomicBool::new(false)),
            reader_quit,
            exited,
            proxy,
        }
    }

    /// The link refused a frame. Every keystroke after the first would say the
    /// same thing, so this side says it once; and unless the reader has been
    /// retired for a relink or a release, the pane is marked exited the way the
    /// reader marks it on EOF — it is the same socket, noticed from the writing
    /// side first — so the window shows the pane as gone instead of taking
    /// input into it that nothing will ever read. The reader still raises its
    /// own `Exit` when it finds the same socket closed; the handler is
    /// idempotent, so a link that is genuinely gone may be reported twice.
    ///
    /// This is hardening for a *closed* link, not the cure for #673: a socket
    /// some process holds open and never reads accepts writes into its send
    /// buffer, so they succeed and vanish until the buffer fills, and nothing
    /// here fires until the backlog bound does. What stops that pane existing
    /// at all is `attach_on` refusing to call a silent `Attach` attached.
    fn note(&self, err: &std::io::Error) {
        if self.said.swap(true, Ordering::SeqCst) {
            return;
        }
        log::warn!("the daemon link stopped taking this pane's input: {err}");
        if self.reader_quit.load(Ordering::SeqCst) {
            return;
        }
        self.exited.store(true, Ordering::SeqCst);
        self.proxy.send_event(AlacEvent::Wakeup);
        self.proxy.send_event(AlacEvent::Exit);
    }
}

#[derive(Default)]
struct SendQueue {
    frames: VecDeque<Vec<u8>>,
    /// Bytes queued but not yet on the wire — including the batch the sender
    /// currently holds, which is why this is not just `frames`' total. That
    /// batch is exactly what a stalled link parks in, so leaving it out would
    /// mean the backlog bound could never be reached.
    bytes: usize,
    /// How much of `bytes` belongs to frames that were over the whole bound on
    /// their own, and so were let through on their own terms. The bound is
    /// raised by exactly this while they are outstanding, and put back the
    /// moment the queue empties — otherwise one big paste would leave every
    /// keystroke behind it looking like a dead link.
    oversize: usize,
    /// Set by teardown, or by a sender that has given up: either way the queue
    /// takes nothing more. Teardown's sender writes what is left first.
    closing: bool,
    /// Set by the sender once there is nothing left to write. Teardown waits
    /// on this for `DRAIN_GRACE`, no longer.
    drained: bool,
}

/// A pane's writing half, moved onto a thread of its own.
///
/// The socket underneath is blocking and has no write timeout, so a peer that
/// stops reading parks `write(2)` in the kernel until it starts again. Every
/// caller of `RemoteTerminal::write` is a gpui event handler — a keystroke, a
/// paste, a mouse report, a focus change — and one UI thread draws every
/// window, so a park there is every window frozen. On macOS a unix stream gives
/// up after 8K of send buffer, which is about 1400 keystrokes: a single paste.
///
/// So nothing on the UI thread touches the socket. Frames are encoded, queued,
/// and handed to a sender thread that is welcome to park for as long as the far
/// end makes it.
struct LinkWriter {
    /// A second handle on the same socket, kept for `shutdown` alone.
    /// `shutdown(2)` returns at once even while another thread is parked in
    /// `write(2)` on that socket — which is exactly the state teardown has to
    /// be able to break, and exactly what a handle behind the sender's own lock
    /// could not do.
    closer: Stream,
    queue: Arc<(Mutex<SendQueue>, std::sync::Condvar)>,
    loss: InputLoss,
    thread: Option<JoinHandle<()>>,
}

impl LinkWriter {
    fn new(stream: Stream, loss: InputLoss) -> std::io::Result<LinkWriter> {
        let closer = stream.try_clone()?;
        let queue = Arc::new((Mutex::new(SendQueue::default()), std::sync::Condvar::new()));
        let sending = Arc::clone(&queue);
        let sender_loss = loss.clone();
        let thread = std::thread::Builder::new()
            .name("tty7-pane-writer".into())
            .spawn(move || send_loop(stream, sending, sender_loss))?;
        Ok(LinkWriter {
            closer,
            queue,
            loss,
            thread: Some(thread),
        })
    }

    /// Queues a frame. Never blocks: the socket belongs to the sender thread,
    /// and the only thing that happens here is a push onto a `VecDeque`.
    fn send(&self, msg: ClientMsg) {
        let mut frame = Vec::new();
        if let Err(e) = msg.encode(&mut frame) {
            log::warn!("could not encode a frame for this pane's link: {e}");
            return;
        }
        let (lock, wake) = &*self.queue;
        let Ok(mut q) = lock.lock() else { return };
        if q.closing {
            return;
        }
        // A frame over the bound all by itself is not a backlog — a paste is
        // whatever the clipboard holds, and refusing a big one would kill a
        // perfectly healthy pane. Onto an empty queue it goes through anyway,
        // and lifts the bound by its own size for as long as it is outstanding,
        // so what piles up behind it is still held to the same four megabytes.
        // Onto a queue that already has something on it, the ordinary bound
        // applies: one such frame is a paste, a second one arriving before the
        // first has moved is a link that is not moving.
        let oversize = frame.len() > MAX_BACKLOG && q.bytes == 0;
        if !oversize && q.bytes + frame.len() > MAX_BACKLOG + q.oversize {
            // Dropped rather than queued: a backlog this deep is a link nothing
            // is reading, and growing it only trades a frozen window for an
            // exhausted heap. Reported in the same words, and once, as a write
            // the link refuses outright — the pane is gone either way.
            drop(q);
            self.loss.note(&std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("nothing has drained this pane's link for {MAX_BACKLOG} bytes of input"),
            ));
            return;
        }
        if oversize {
            q.oversize = frame.len();
        }
        q.bytes += frame.len();
        q.frames.push_back(frame);
        q.drained = false;
        drop(q);
        wake.notify_one();
    }

    /// Retires the sender and cuts the link. Gives what is queued `DRAIN_GRACE`
    /// to go out — see the constant — and then shuts the socket down whether it
    /// went or not. Never joins: a sender parked on a dead link would take the
    /// UI thread down with it, the same trap `stop_reader` documents.
    ///
    /// Called twice on the way out — `stop_reader` closes the link, then the
    /// field drop closes it again — so the second call has to be free rather
    /// than another `DRAIN_GRACE` spent waiting for a sender that is already
    /// gone. A retired handle is one whose thread has been let go.
    fn close(&mut self) {
        if self.thread.is_none() {
            return;
        }
        let (lock, wake) = &*self.queue;
        if let Ok(mut q) = lock.lock() {
            q.closing = true;
            wake.notify_one();
            if let Ok((waited, _)) = wake.wait_timeout_while(q, DRAIN_GRACE, |q| !q.drained) {
                drop(waited);
            }
        }
        let _ = self.closer.shutdown(std::net::Shutdown::Both);
        drop(self.thread.take());
    }
}

impl Drop for LinkWriter {
    fn drop(&mut self) {
        self.close();
    }
}

fn send_loop(
    mut stream: Stream,
    queue: Arc<(Mutex<SendQueue>, std::sync::Condvar)>,
    loss: InputLoss,
) {
    use std::io::Write as _;
    let (lock, wake) = &*queue;

    // Abandons whatever is still queued and reports the link settled. Wakes a
    // teardown that is inside `DRAIN_GRACE` waiting to hear it: what is left
    // here is never going out, and there is nothing to be gained by making the
    // window sit out the rest of the grace period to find that out. Closes the
    // queue on the way, too — with no thread left to drain it, anything queued
    // after this is just a keystroke held onto until the pane drops.
    let give_up = || {
        if let Ok(mut q) = lock.lock() {
            q.frames.clear();
            q.bytes = 0;
            q.oversize = 0;
            q.closing = true;
            q.drained = true;
            wake.notify_all();
        }
    };

    loop {
        let batch = {
            let Ok(mut q) = lock.lock() else { return };
            loop {
                if !q.frames.is_empty() {
                    break;
                }
                // Nothing left to write, so the link is as flushed as this
                // thread can make it. Said before the `closing` check, so a
                // teardown racing a sender that has already finished hears it
                // rather than waiting out `DRAIN_GRACE` for nothing.
                q.drained = true;
                wake.notify_all();
                if q.closing {
                    return;
                }
                let Ok(next) = wake.wait(q) else { return };
                q = next;
            }
            std::mem::take(&mut q.frames)
        };
        // Counted against the backlog until it is actually out. Discounting it
        // at the moment it left the `VecDeque` would let a sender parked on the
        // first frame of a huge batch hold the whole thing off the books, and
        // the bound the batch is meant to enforce would never be reached.
        let taken: usize = batch.iter().map(Vec::len).sum();
        // Written with the lock released: parking here is the whole point, and
        // a sender holding the queue lock while it parked would put every
        // `send` — every keystroke — behind the same wait it exists to absorb.
        for frame in batch {
            if let Err(e) = stream.write_all(&frame) {
                loss.note(&e);
                give_up();
                return;
            }
        }
        if let Err(e) = stream.flush() {
            loss.note(&e);
            give_up();
            return;
        }
        if let Ok(mut q) = lock.lock() {
            q.bytes = q.bytes.saturating_sub(taken);
            if q.bytes == 0 {
                q.oversize = 0;
            }
        }
    }
}

pub struct RemoteTerminal {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub events: smol::channel::Receiver<AlacEvent>,
    pub palette: [alacritty_terminal::vte::ansi::Rgb; 256],
    pub exited: bool,
    size: TermSize,
    synced_size: bool,
    /// This link reattached to a pane that was already running rather than
    /// spawning a fresh one, so whatever it reports first — an agent in the
    /// foreground included — predates this client.
    reattached: bool,
    /// The `(cell_w, cell_h)` last sent to the daemon, in device pixels. Tracked
    /// alongside `size` so a display-scale change still reaches the child even
    /// when the grid dimensions are unchanged.
    synced_cell: (u16, u16),
    /// When the last `ClientMsg::Resize` went down the link. On an echoing
    /// route it is what tells "the echo is still in flight" apart from "the
    /// echo is never coming" — see [`RESIZE_ECHO_GRACE`].
    resize_sent_at: Option<std::time::Instant>,
    link: LinkWriter,
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
    agent_session: Arc<Mutex<AgentSlot>>,
    /// Kitty-graphics images placed on this pane's grid (issue #213).
    /// Written by the reader thread from out-of-band `Image`/`DeleteImage`
    /// frames, read by the paint path — only the client holds the grid the
    /// anchors are relative to, so the store lives here rather than in the daemon.
    images: crate::terminal::images::ImageStore,
    clipboard_writes: Arc<Mutex<VecDeque<tty7_core::core::clipboard::ClipboardWrite>>>,
    clipboard_write_busy: Arc<AtomicBool>,
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
    /// Whether this pane's pty is a ConPTY, for input encoding and repairing
    /// the cursor a repaint parks. Held here so a relink hands the same answer
    /// to the reader it starts: a pane's pty does not change kind when the link
    /// to it is rebuilt, and the route a relink carries cannot tell a
    /// native-SSH pane from a local shell.
    local_conpty: Arc<AtomicBool>,
}

/// The workspace id a spawn carries, so the pane's shell gets `$TTY7_WS` and a
/// CLI running in it can use workspace-scoped verbs with no argument.
///
/// This is the same id as `owner`, but decided separately: `owner` is also
/// gated on FEATURE_PANE_OWNER, while the workspace field rides the owned spawn
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
            allow_remote_clipboard_write: route.allow_remote_clipboard_write(),
        }
        .encode(&mut stream)?;
        let pane_id = match spawn_reply(&mut stream, attach_reply_wait(route), "Spawn")? {
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

        let mut term =
            Self::from_stream_with(stream, size, Vec::new(), PtySource::for_route(route))?;
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
    pub(crate) fn seed_cwd(&self, cwd: Option<PathBuf>) {
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

        ClientMsg::Attach {
            pane_id,
            size: win,
            allow_remote_clipboard_write: route.allow_remote_clipboard_write(),
        }
        .encode(&mut stream)?;
        let buffered = match attach_reply_prefix(&mut stream, pane_id, attach_reply_wait(route)) {
            Ok(buffered) => buffered,
            Err(e) if route.is_local() && attach_unanswered(&e) => {
                // Silence on the attach socket has two readings, and only one
                // of them is safe to act on. Ask the daemon on a fresh
                // connection: if it answers `Version` there, it is up and
                // serving, and the attach connection is one it will never
                // serve — a socket some process holds open, or one from a
                // listener no longer drained — so the verdict stands and the
                // caller spawns fresh. If it does not answer there either, it
                // is not serving anyone yet — mid-restart, mid-handoff — and
                // a fresh pane spawned now would land on a live one the moment
                // it comes up (its history carried across, its agent session
                // resumed twice). There is no third path from a synchronous
                // UI-thread call, so the attach still fails, but says which
                // silence it was: the log line someone reads while diagnosing
                // an orphaned shell must not claim the pane was gone.
                //
                // Only local routes probe: a remote attach already waits 15 s
                // and a second routed connection is a second bridge process.
                if local_daemon_answers() {
                    return Err(e);
                }
                return Err(e.context(
                    "the daemon answered nothing on a fresh connection either — it is not \
                     serving yet (restarting?), so this pane may still be alive",
                ));
            }
            Err(e) => return Err(e),
        };
        let mut term =
            Self::from_stream_parts(stream, size, buffered, PtySource::for_route(route), true)?;
        term.route = route.clone();
        Ok(term)
    }

    /// Opens the daemon connection a relink will adopt, and waits for the
    /// daemon's verdict on the `Attach`. Bytes read past the verdict are
    /// returned so the adopted reader loses none of the replay. Waiting here
    /// is what makes a relink's failure classifiable: fire-and-forget handed
    /// a "no such pane" refusal to the reader thread, which could only die
    /// with it — indistinguishable from the link dropping again.
    pub fn open_relink(
        route: &PaneRoute,
        pane_id: u64,
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
    ) -> anyhow::Result<(Stream, Vec<u8>)> {
        let mut stream = connect_routed(route)?;
        ClientMsg::Attach {
            pane_id,
            size: win_size(size, cell_w, cell_h),
            allow_remote_clipboard_write: route.allow_remote_clipboard_write(),
        }
        .encode(&mut stream)?;
        let buffered = attach_reply_prefix(&mut stream, pane_id, attach_reply_wait(route))?;
        Ok((stream, buffered))
    }

    pub fn adopt_relink(
        &mut self,
        stream: Stream,
        buffered: Vec<u8>,
        route: &PaneRoute,
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
    ) -> anyhow::Result<()> {
        self.stop_reader();
        while self.events.try_recv().is_ok() {}
        if let Ok(mut pending) = self.clipboard_writes.lock() {
            pending.clear();
        }
        self.clipboard_write_busy.store(false, Ordering::Release);

        let read_half = stream.try_clone()?;

        // A relink keeps the view, and the view keeps the status it last saw
        // before the link dropped. That is already a baseline, and the better
        // one: if the agent finished its turn while the link was down, the
        // daemon's replayed `Done` against the view's `Working` is exactly the
        // edge the reader must be told about. So the relink's replay is read
        // as a live report, and a cold-attach mark nobody took yet is dropped
        // rather than left to swallow that edge.
        if let Ok(mut guard) = self.agent_session.lock() {
            guard.replayed = false;
        }

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
            buffered,
            quit.clone(),
            ReaderSignals {
                cwd: self.cwd.clone(),
                shell: self.shell_state.clone(),
                remote: self.remote_context.clone(),
                agent: self.agent.clone(),
                agent_session: self.agent_session.clone(),
                // The daemon does replay the stored status down this link, but
                // the view already has a baseline from before the drop — see
                // above.
                awaiting_replay: false,
                exited: self.exited_flag.clone(),
                child_exited: self.child_exited.clone(),
                zle_reading: self.zle_reading.clone(),
                shell_vi_mode: self.shell_vi_mode.clone(),
                running_command: self.running_command.clone(),
                auth: self.auth_prompts.clone(),
                phase: self.ssh_phase.clone(),
                images: self.images.clone(),
                clipboard_writes: self.clipboard_writes.clone(),
                clipboard_write_busy: self.clipboard_write_busy.clone(),
                // Deliberately the pane's existing answer rather than one
                // rebuilt from `route`: the pty on the far side is the same pty
                // it was before the link dropped, and only this value still
                // remembers what a `RemoteContext` taught the old reader.
                local_conpty: self.local_conpty.clone(),
            },
        );
        self.reader_thread = Some(reader);
        self.reader_quit = quit;
        // Installed after `reader_quit`, so the sender reports a refusal
        // against the reader this link actually has. Assigning retires the old
        // `LinkWriter` through its `Drop`, which is what closes the old socket.
        self.link = LinkWriter::new(stream, self.input_loss())?;
        self.route = route.clone();
        self.synced_size = false;
        self.resize(size, cell_w, cell_h);
        Ok(())
    }

    /// A pane the client *reattached* to rather than spawned — the shape the
    /// app restores last session's tabs in, where the head of the stream is the
    /// daemon replaying state the pane already had.
    #[cfg(test)]
    pub(super) fn from_stream_reattached(stream: Stream, size: TermSize) -> anyhow::Result<Self> {
        Self::from_stream_parts(
            stream,
            size,
            Vec::new(),
            PtySource::for_route(&PaneRoute::Local),
            true,
        )
    }

    /// A pane on a pty of this machine's own — what the tests build, and what
    /// `spawn_on` narrows with the route it dialled.
    pub(super) fn from_stream(stream: Stream, size: TermSize) -> anyhow::Result<Self> {
        Self::from_stream_with(
            stream,
            size,
            Vec::new(),
            PtySource::for_route(&PaneRoute::Local),
        )
    }

    pub(super) fn from_stream_with(
        stream: Stream,
        size: TermSize,
        buffered: Vec<u8>,
        pty: PtySource,
    ) -> anyhow::Result<Self> {
        Self::from_stream_parts(stream, size, buffered, pty, false)
    }

    /// `awaiting_replay` says this link is an attach rather than a spawn, and
    /// so that the frames at the head of its stream describe a pane that was
    /// already running — see [`AgentSlot`]. It has to be decided here rather
    /// than set on the returned terminal: the reader starts inside this
    /// function, and against a daemon that answers promptly the replay can be
    /// parsed before the caller gets its value back.
    fn from_stream_parts(
        stream: Stream,
        size: TermSize,
        buffered: Vec<u8>,
        pty: PtySource,
        awaiting_replay: bool,
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
        let agent_session: Arc<Mutex<AgentSlot>> = Arc::new(Mutex::new(AgentSlot::default()));
        let exited_flag = Arc::new(AtomicBool::new(false));
        let child_exited = Arc::new(AtomicBool::new(false));
        let zle_reading = Arc::new(AtomicBool::new(false));
        let shell_vi_mode = Arc::new(AtomicBool::new(false));
        let running_command: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let auth_prompts: Arc<Mutex<VecDeque<(u64, AuthPromptKind)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let ssh_phase: Arc<Mutex<Option<SshPhase>>> = Arc::new(Mutex::new(None));
        let images = crate::terminal::images::ImageStore::new();
        let clipboard_writes = Arc::new(Mutex::new(VecDeque::new()));
        let clipboard_write_busy = Arc::new(AtomicBool::new(false));

        let reader_quit = Arc::new(AtomicBool::new(false));
        let local_conpty = Arc::new(AtomicBool::new(pty == PtySource::LocalConpty));
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
                awaiting_replay,
                exited: exited_flag.clone(),
                child_exited: child_exited.clone(),
                zle_reading: zle_reading.clone(),
                shell_vi_mode: shell_vi_mode.clone(),
                running_command: running_command.clone(),
                auth: auth_prompts.clone(),
                phase: ssh_phase.clone(),
                images: images.clone(),
                clipboard_writes: clipboard_writes.clone(),
                clipboard_write_busy: clipboard_write_busy.clone(),
                local_conpty: local_conpty.clone(),
            },
        );

        let link = LinkWriter::new(
            write_half,
            InputLoss::new(reader_quit.clone(), exited_flag.clone(), proxy.clone()),
        )?;

        Ok(Self {
            term,
            events: rx,
            palette: super::palette::build(),
            exited: false,
            size,
            synced_size: false,
            reattached: awaiting_replay,
            synced_cell: (0, 0),
            resize_sent_at: None,
            link,
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
            clipboard_writes,
            clipboard_write_busy,
            route: PaneRoute::Local,
            proxy,
            reader_thread: Some(reader_thread),
            reader_quit,
            local_conpty,
        })
    }

    /// The signals the sender thread raises a refused frame through, bundled
    /// for the `LinkWriter` about to be installed. Read after `reader_quit` has
    /// been swapped, so a relink's new sender answers to the new reader.
    fn input_loss(&self) -> InputLoss {
        InputLoss::new(
            self.reader_quit.clone(),
            self.exited_flag.clone(),
            self.proxy.clone(),
        )
    }

    pub fn detach_link(&mut self) {
        self.link.send(ClientMsg::Detach);
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
        // The sender owns the socket now, and closing it is its job: `close`
        // gives whatever is queued a brief moment to go out and then shuts the
        // socket down regardless, which is also what wakes the reader.
        self.link.close();
        drop(self.reader_thread.take());
    }

    pub fn apply_user_config(&self, user_config: &crate::core::config::Config) {
        let mut term = self.term.lock();
        term.set_options(terminal_config_from_user(user_config));
    }

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
                    awaiting_replay,
                    exited: exited_flag,
                    child_exited,
                    zle_reading,
                    shell_vi_mode,
                    running_command,
                    auth,
                    phase,
                    images,
                    clipboard_writes,
                    clipboard_write_busy,
                    local_conpty,
                } = signals;
                let mut awaiting_replay = awaiting_replay;
                crate::core::threads::promote_to_user_interactive();
                let mut stream = read_half;
                let mut processor: ansi::Processor = ansi::Processor::new();
                let mut osc = OscNotifyScanner::default();
                let mut mode_tok = OscTokenizer::new(&[b"133"]);
                let mut zle_tok = OscTokenizer::new(&[b"133"]);
                // #889: an OSC 0/2 the emulator above has already adopted, read
                // a second time only to learn whether the program that wrote it
                // has since exited. `TitleLifetime` is the daemon's rule too,
                // so a window's tab strip and the switcher reading the tree can
                // never disagree about whether a title is still current.
                let mut title_tok = OscTokenizer::new(&[b"0", b"2", b"133"]);
                let mut title_life = TitleLifetime::default();
                // No live `Output` frame yet: whatever arrives now is the
                // daemon's replay (an attach or a relink), which it sends
                // entirely as `Snapshot`s followed by the stored state.
                let mut replaying_state = true;
                let mut cursor_scan = ParkedCursorScanner::new();
                let mut parked_cursor = ParkedCursorRepair::default();
                // #837: the command marks, cut at exactly where they land so
                // the cursor style a finished command left behind is judged
                // against the bytes before its `D`, not the whole batch.
                let mut command_tok = OscTokenizer::new(&[b"133"]);
                let mut command_cursor = CommandCursorStyle::default();
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

                let teardown = |reason: Option<&str>| {
                    // A retired reader exits silently: the pane is being
                    // relinked or released, not dying — marking it exited
                    // would wrongly kill the freshly adopted link.
                    if quit.load(Ordering::SeqCst) {
                        return;
                    }
                    // Said out loud because every way a link dies looks the
                    // same from the window — a pane suddenly "disconnected" —
                    // and this line is the only record of which way it was.
                    if let Some(reason) = reason {
                        log::warn!("a pane's link died: {reason}");
                    }
                    term.lock().exit();
                    exited_flag.store(true, Ordering::SeqCst);
                    proxy.send_event(AlacEvent::Wakeup);
                    proxy.send_event(AlacEvent::Exit);
                };

                let mut out_batch: Vec<u8> = Vec::new();

                // Whether this chunk ends with the title the pane is showing
                // belonging to a command that has finished. The emulator has
                // already parsed the same bytes, so a `true` here is sent on
                // as a `ResetTitle` *after* every `Title` the chunk produced —
                // which is the whole ordering question: a shell that re-titles
                // itself in `precmd` writes its OSC 0/2 after the `D`, and
                // that title is the last word rather than a thing to undo.
                macro_rules! chunk_retires_the_title {
                    ($bytes:expr) => {{
                        let mut retire = false;
                        title_tok.feed($bytes, |payload| match title_life.saw(payload) {
                            TitleEffect::Retire => retire = true,
                            TitleEffect::Set => retire = false,
                            TitleEffect::None => {}
                        });
                        retire
                    }};
                }

                'main: loop {
                    macro_rules! flush_batch {
                        () => {
                            if !out_batch.is_empty() {
                                // The scanner reports an offset one past each
                                // sequence it matched, in ascending order, so
                                // the batch splits at each of them: advance the
                                // emulator to the cut, act on the state that
                                // sequence left behind, carry on.
                                let mut cuts: Vec<(usize, ReaderCut)> = Vec::new();
                                if local_conpty.load(Ordering::Relaxed) {
                                    cursor_scan.feed(&out_batch, |off, c| {
                                        cuts.push((off, ReaderCut::Parked(c)))
                                    });
                                }
                                command_cuts(&mut command_tok, &out_batch, &mut cuts);
                                {
                                    let t0 = trace.then(std::time::Instant::now);
                                    let Some(waited) = feed_grid(
                                        &term,
                                        &mut processor,
                                        &out_batch,
                                        cuts,
                                        &quit,
                                        |term, cut| match cut {
                                            ReaderCut::Parked(cut) => {
                                                parked_cursor.apply(term, cut)
                                            }
                                            ReaderCut::Command(mark) => {
                                                command_cursor.apply(term, mark)
                                            }
                                        },
                                    ) else {
                                        return;
                                    };
                                    if let Some(t0) = t0 {
                                        tr_lock_t += waited;
                                        tr_adv_t += t0.elapsed() - waited;
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
                                if chunk_retires_the_title!(&out_batch) {
                                    proxy.send_event(AlacEvent::ResetTitle);
                                }
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
                            Err(e) => {
                                teardown(Some(&format!("unframeable bytes on the link: {e}")));
                                break 'main;
                            }
                        };
                        let msg = match DaemonMsg::from_frame(frame.0, frame.1) {
                            Ok(msg) => msg,
                            Err(e) => {
                                teardown(Some(&format!("undecodable frame on the link: {e}")));
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
                                // A live echo lands just before the shell's
                                // SIGWINCH redraw; a replayed Size heads old
                                // bytes that nothing is going to repaint.
                                let shell_redraws = !replaying_state
                                    && !local_conpty.load(Ordering::Relaxed)
                                    && shell.lock().is_ok_and(|s| s.active && s.at_prompt);
                                {
                                    let mut term = term.lock();
                                    if quit.load(Ordering::SeqCst) {
                                        return;
                                    }
                                    super::prompt_reflow::resize(
                                        &mut term,
                                        TermSize::new(ws.cols as usize, ws.rows as usize),
                                        shell_redraws,
                                    );
                                }
                                proxy.send_event(AlacEvent::Wakeup);
                            }
                            DaemonMsg::Snapshot(bytes) => {
                                flush_batch!();
                                cursor_scan.reset();
                                parked_cursor.reset();
                                proxy.replaying.store(true, Ordering::Relaxed);
                                // The replay carries the command marks too, so
                                // a pane reattached after `nvim` exited in it
                                // comes back with the prompt's cursor.
                                let mut cuts: Vec<(usize, ReaderCut)> = Vec::new();
                                command_cuts(&mut command_tok, &bytes, &mut cuts);
                                let fed =
                                    feed_grid(&term, &mut processor, &bytes, cuts, &quit, |term, cut| {
                                        if let ReaderCut::Command(mark) = cut {
                                            command_cursor.apply(term, mark);
                                        }
                                    });
                                if fed.is_none() {
                                    return;
                                }
                                if processor.sync_timeout().sync_timeout().is_some() {
                                    let mut term = term.lock();
                                    if quit.load(Ordering::SeqCst) {
                                        return;
                                    }
                                    processor.stop_sync(&mut *term);
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
                                // The replay carries the pane's recent marks,
                                // so reading it is what lets a reattached
                                // window know whether the title it just
                                // adopted belongs to anything still running.
                                if chunk_retires_the_title!(&bytes) {
                                    proxy.send_event(AlacEvent::ResetTitle);
                                }
                                proxy.send_event(AlacEvent::Wakeup);
                            }
                            DaemonMsg::Output(bytes) => {
                                // Live output only ever follows the whole
                                // replay (the daemon sends the stored status
                                // last, and the stream keeps that order), so
                                // the first frame here ends the window in which
                                // a status can still be a replayed one. Without
                                // this, a pane that had no agent session to
                                // replay would keep the window open until some
                                // agent it ran *later* reported for the first
                                // time, and that report would be discounted.
                                awaiting_replay = false;
                                replaying_state = false;
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
                            DaemonMsg::ClipboardWrite(frame) => {
                                flush_batch!();
                                if let Some(write) =
                                    tty7_core::core::clipboard::ClipboardWrite::decode_frame(frame)
                                {
                                    if clipboard_write_busy
                                        .compare_exchange(
                                            false,
                                            true,
                                            Ordering::AcqRel,
                                            Ordering::Acquire,
                                        )
                                        .is_ok()
                                    {
                                        if let Ok(mut pending) = clipboard_writes.lock() {
                                            pending.push_back(write);
                                            proxy.send_event(AlacEvent::Wakeup);
                                        } else {
                                            clipboard_write_busy.store(false, Ordering::Release);
                                        }
                                    } else {
                                        let reply = tty7_core::core::clipboard::response(
                                            write.id.as_deref(),
                                            "EBUSY",
                                        );
                                        proxy.send_event(AlacEvent::PtyWrite(
                                            String::from_utf8(reply)
                                                .expect("OSC 5522 replies are ASCII"),
                                        ));
                                    }
                                } else {
                                    clipboard_write_busy.store(false, Ordering::Release);
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
                                // The replay says a command owns the pane
                                // right now. If the ring it replayed had
                                // already rolled past that command's `C`, the
                                // title just adopted from it is the command's
                                // and its `D` has to retire it (#889).
                                if replaying_state && active && !at_prompt {
                                    title_life.joined_mid_command();
                                }
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
                                // A native-SSH pane's pty is the far host's,
                                // however local the daemon that dialled it: the
                                // daemon bridges an ssh channel straight through
                                // and opens no ConPTY of its own. The route
                                // cannot say so — such a pane is spawned and
                                // attached through the local daemon like any
                                // other — so this frame is where a client that
                                // reopened onto an existing one finds out. A
                                // one-way latch: the far end of an ssh channel
                                // never becomes a local pty later, while an
                                // `ssh` *command* (RemoteKind::Ssh) is a program
                                // inside a ConPTY and keeps the repair.
                                if ctx
                                    .as_ref()
                                    .is_some_and(|c| c.kind == RemoteKind::NativeSsh)
                                {
                                    local_conpty.store(false, Ordering::Relaxed);
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
                                    guard.state = state;
                                    // The first such frame on an attached link
                                    // is the pane's stored status being
                                    // replayed, not a turn changing state now.
                                    guard.replayed = std::mem::take(&mut awaiting_replay);
                                }
                                proxy.send_event(AlacEvent::Wakeup);
                            }
                            DaemonMsg::Exited { .. } => {
                                flush_batch!();
                                child_exited.store(true, Ordering::SeqCst);
                                // Routine — the child ended — so no log line.
                                teardown(None);
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
                            teardown(Some("the far end closed it (EOF)"));
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
                        Err(e) => {
                            teardown(Some(&format!("read failed: {e}")));
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

    pub(super) fn is_local_conpty(&self) -> bool {
        self.local_conpty.load(Ordering::Relaxed)
    }

    /// Queues a keystroke — or a paste, or a mouse report — for the link.
    ///
    /// Callers are gpui event handlers on the UI thread, so this returns
    /// without touching the socket. A link that has stopped draining is a
    /// problem for the sender thread, not for the window.
    pub fn write<B: Into<Cow<'static, [u8]>>>(&self, bytes: B) {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return;
        }
        self.link.send(ClientMsg::Input(bytes.into_owned()));
    }

    /// Whether the daemon behind this pane echoes a `DaemonMsg::Size` into the
    /// output stream when it applies our `ClientMsg::Resize`. When it does, the
    /// local grid reflow is deferred to that echo on the reader thread: any
    /// backlog still queued between daemon and client was produced at the old
    /// geometry, and reflowing before it drains parses old-width bytes into a
    /// new-width grid (maximize during a burst of output garbled the pane).
    ///
    /// The local daemon is probed directly. A routed pane consults the answer
    /// its route carries, read off the host's control hello when the route was
    /// built — the daemon serves one `ClientMsg` per connection, so asking
    /// `Version` per pane would cost a whole routed connection each time. The
    /// remaining cases never defer: an older server does not advertise the
    /// echo, an unroutable route reaches no daemon at all, and deferral must
    /// not hang the grid on an echo nobody promised.
    fn resize_echoed(&self) -> bool {
        match &self.route {
            PaneRoute::Local => crate::daemon::spawn::local_daemon_supports(
                crate::daemon::protocol::FEATURE_RESIZE_ECHO,
            ),
            PaneRoute::Remote { resize_echo, .. } => *resize_echo,
            PaneRoute::Unroutable(_) => false,
        }
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
            // Already asked for, so the only question is whether the grid got
            // there. It is not enough to trust the request (#893): on an
            // echoing route the grid moves only when the daemon's `Size` comes
            // back, and a request or an echo that went missing left a pane
            // painting a grid shorter than its bounds — the bottom rows blank,
            // the child still drawing for the old size — with every later
            // frame asking for the same size and being skipped here, until a
            // divider drag asked for a different one.
            if echoed {
                // Disagreement while an echo can still be on its way is just
                // the echo (or a replay segment mid-apply) not having landed
                // yet. Asked first because it needs no lock.
                if self
                    .resize_sent_at
                    .is_some_and(|at| at.elapsed() < RESIZE_ECHO_GRACE)
                {
                    return;
                }
                // Tried rather than waited for, as the painter does: this runs
                // every frame, and a held lock is the reader mid-batch — the
                // next frame asks again.
                match self.term.try_lock_unfair() {
                    Some(term) if !grid_holds(&term, size) => {}
                    _ => return,
                }
            } else if grid_holds(&self.term.lock(), size) {
                return;
            }
        }
        self.synced_size = true;
        self.size = size;
        self.synced_cell = cell;
        if !echoed {
            let shell_redraws = !self.is_local_conpty() && self.at_prompt();
            super::prompt_reflow::resize(&mut self.term.lock(), size, shell_redraws);
        }

        let win = win_size(size, cell_w, cell_h);
        self.link.send(ClientMsg::Resize(win));
        self.resize_sent_at = Some(std::time::Instant::now());
    }

    pub fn foreground_cwd(&self) -> Option<PathBuf> {
        self.cwd.lock().ok().and_then(|g| g.clone())
    }

    pub fn remote_context(&self) -> Option<RemoteContext> {
        self.remote_context.lock().ok().and_then(|g| g.clone())
    }

    /// See the field: an attach to a running pane, not a fresh spawn.
    pub fn reattached(&self) -> bool {
        self.reattached
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

    pub fn pop_clipboard_write(&self) -> Option<tty7_core::core::clipboard::ClipboardWrite> {
        self.clipboard_writes
            .lock()
            .ok()
            .and_then(|mut pending| pending.pop_front())
    }

    pub fn finish_clipboard_write(&self) {
        self.clipboard_write_busy.store(false, Ordering::Release);
    }

    pub fn agent_session(&self) -> Option<AgentSessionState> {
        self.agent_session.lock().ok().and_then(|g| g.state.clone())
    }

    /// The status the daemon replayed when this link attached, handed out once.
    ///
    /// `Some(status)` means what [`Self::agent_session`] reports right now is
    /// stored state from before this client existed — the caller should take it
    /// as its starting point, not as something that just happened. Answering
    /// only once is what keeps the very next live transition an edge again.
    pub fn take_replayed_agent_status(&self) -> Option<Option<AgentStatus>> {
        let mut guard = self.agent_session.lock().ok()?;
        if !guard.replayed {
            return None;
        }
        guard.replayed = false;
        Some(guard.state.as_ref().map(|s| s.status))
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
        // Native SSH is always dialled through the local daemon, whatever the
        // far end turns out to be, so this waits on the local budget.
        let pane_id = match spawn_reply(
            &mut stream,
            attach_reply_wait(&PaneRoute::Local),
            "SpawnNativeSsh",
        )? {
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

        // The local daemon dialled this one, but it opened no pty for it: the
        // pane is an ssh channel bridged straight through, so the bytes are the
        // far host's raw pty and no conhost ever sees them.
        let mut term = Self::from_stream_with(stream, size, Vec::new(), PtySource::Raw)?;
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
        self.link.send(ClientMsg::AuthResponse {
            request_id,
            response,
        });
    }

    /// The client half of known-hosts management.
    ///
    /// Nothing calls this yet: there is no known-hosts surface in the window or
    /// the CLI. What sits behind it is not a stub, though — `ssh::known_hosts`
    /// parses the real file, fingerprints each key, and rewrites through a
    /// 0600 temp file — so this is an interface waiting for a screen, not
    /// scaffolding around nothing. Deleting it would throw away the finished
    /// half of the feature.
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

    /// Ask the daemon to dial this spec and say what happened. Blocking, and
    /// bounded by the spec's own connect timeout on the far side — call it off
    /// the UI thread.
    pub fn test_ssh(spec: Box<NativeSshSpec>) -> SshTestReport {
        fn query(spec: Box<NativeSshSpec>) -> anyhow::Result<SshTestReport> {
            let mut stream = connect()?;
            ClientMsg::TestSsh { spec }.encode(&mut stream)?;
            match DaemonMsg::read(&mut stream)? {
                DaemonMsg::SshTestResult(report) => Ok(report),
                other => Err(anyhow::anyhow!("unexpected reply to TestSsh: {other:?}")),
            }
        }
        query(spec).unwrap_or_else(|e| SshTestReport::Failed {
            reason: e.to_string(),
        })
    }

    /// See [`Self::list_known_hosts`] — same story, and it returns the list
    /// after the removal so a caller can redraw from one round trip.
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

    /// The list after switching one forward on or off, or why it was not
    /// switched — the daemon's own refusal, which names what is in the way.
    pub fn set_forward_enabled(
        pane_id: u64,
        forward_id: u64,
        enabled: bool,
    ) -> anyhow::Result<Vec<ManagedForward>> {
        let mut stream = connect()?;
        ClientMsg::SetForwardEnabled {
            pane_id,
            forward_id,
            enabled,
        }
        .encode(&mut stream)?;
        match DaemonMsg::read(&mut stream)? {
            DaemonMsg::ForwardList(list) => Ok(list),
            DaemonMsg::Error(msg) => Err(anyhow::anyhow!(msg)),
            other => Err(anyhow::anyhow!(
                "unexpected reply to SetForwardEnabled: {other:?}"
            )),
        }
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

/// How long an `Attach` may stay silent before it is called unanswered.
///
/// Only a silent connection ever pays this: a dead pane answers `Error` at
/// once and a live one answers `Size` at once, so the budget is not a cost in
/// the common case. What bounds it is the caller — a local restore attaches
/// synchronously on the UI thread, one pane after another, so N silent panes
/// hold the window still for N times this. What argues for more is the other
/// side of the same silence: a daemon merely slow to serve — an execve handoff
/// keeps the listener and its backlog across the exec, and a fresh daemon is
/// adopting panes and seeding ids before it can take an `Attach` — would have
/// served this connection a moment later, and calling it unanswered spawns a
/// fresh pane over a live one, carries the live pane's history onto it and
/// starts an agent resume against a session the old process still holds. The
/// [`AttachUnanswered`] verdict is confirmed against that case in `attach_on`
/// rather than by waiting longer here.
fn attach_reply_wait(route: &PaneRoute) -> std::time::Duration {
    match route.is_local() {
        true => std::time::Duration::from_secs(2),
        false => std::time::Duration::from_secs(15),
    }
}

/// Read the daemon's answer to a spawn request, under the deadline `Attach`
/// uses for the same route.
///
/// A daemon caught mid-restart accepts the connection and then never serves
/// it. `Attach` has been guarded against that silence since #673, and core's
/// `PaneSession::spawn_over` bounds the identical exchange, but this path read
/// with no deadline at all — and the local route spawns synchronously on the
/// UI thread (`ui::app`'s `PaneRoute::Local` branch), so a daemon that went
/// quiet froze the whole window on "new tab".
///
/// The timeout is reported as a plain message with no `io::Error` in its
/// chain, which is what keeps `daemon_disconnected_before_spawn_reply` from
/// claiming it: silence is not a hangup, and retrying it would only wait
/// again. `what` names the request, since the window shows only this text.
fn spawn_reply(
    stream: &mut Stream,
    wait: std::time::Duration,
    what: &str,
) -> anyhow::Result<DaemonMsg> {
    let _ = stream.set_read_timeout(Some(wait));
    let reply = DaemonMsg::read(stream);
    let _ = stream.set_read_timeout(None);
    match reply {
        Ok(msg) => Ok(msg),
        Err(e) if would_block(&e) => Err(anyhow::anyhow!("no answer to {what} within {wait:?}")),
        Err(e) => {
            Err(anyhow::Error::new(e).context(format!("reading the daemon's answer to {what}")))
        }
    }
}

/// An `Attach` that produced no bytes within its wait — nobody served the
/// connection. Distinct from a refusal (`Error` frame) and from a hangup so
/// the caller can say the true thing: the pane may well still exist.
#[derive(Debug)]
struct AttachUnanswered {
    pane_id: u64,
    wait: std::time::Duration,
}

impl std::fmt::Display for AttachUnanswered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the daemon did not answer Attach for pane {} within {:?} (an attached pane replays \
             its screen at once, so nobody is serving this connection)",
            self.pane_id, self.wait
        )
    }
}

impl std::error::Error for AttachUnanswered {}

/// Whether `err` is an `Attach` that went unanswered, as opposed to refused or
/// hung up on. The pane behind an unanswered attach may still be alive.
pub fn attach_unanswered(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<AttachUnanswered>().is_some())
}

/// An `Attach` the daemon answered with an `Error` frame — it heard us and
/// said no, which for a relink means the pane is gone on its machine. Typed so
/// a retry loop can tell this apart from the failures worth retrying: silence
/// and transport trouble pass, a refusal repeats identically forever.
#[derive(Debug)]
struct AttachRefused {
    message: String,
}

impl std::fmt::Display for AttachRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "daemon refused Attach: {}", self.message)
    }
}

impl std::error::Error for AttachRefused {}

/// Whether `err` is an `Attach` the daemon explicitly refused. Retrying one of
/// these can only produce the same refusal.
pub fn attach_refused(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<AttachRefused>().is_some())
}

/// Reads far enough into the daemon's answer to an `Attach` to classify it, and
/// hands back every byte read so the reader thread loses none of the replay.
///
/// Silence is a verdict, not a quiet pane. A daemon that attached replays the
/// pane's ring before it reads a byte of our input, and the ring always holds
/// a segment, so the first frame on a good attach is a `Size` — a quiet pane
/// still sends that. An `Attach` that produced nothing within `wait` is one
/// nobody is serving: a daemon still mid-restart, a socket held open by a
/// process that will never read it. Taken for success it gave the window a
/// pane drawn as restored whose every keystroke went into that socket — no
/// fresh shell, no restored-screen banner, no agent resume, and Ctrl-C did
/// nothing (#673). Taken for the failure it is, the caller spawns fresh and
/// asks for the old screen back.
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
    if buffered.is_empty() {
        return Err(anyhow::Error::new(AttachUnanswered { pane_id, wait }));
    }
    if !kind.is_some_and(crate::daemon::protocol::is_error_kind) {
        return Ok(buffered);
    }
    let message = read_error_frame(stream, &mut buffered, wait)
        .unwrap_or_else(|| format!("no such pane {pane_id}"));
    Err(anyhow::Error::new(AttachRefused { message }))
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
        self.link.send(ClientMsg::Detach);
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
/// macOS always goes through `macos_notify`, clickable or not. Elsewhere, every
/// other case (no pane, unsupported platform, Windows toast queue full) falls
/// back to the plain `notify-rust` path below, which is why
/// `try_clickable_notification` reports whether it took the job.
pub(crate) fn notify_desktop_for_pane(title: Option<&str>, body: &str, pane: Option<EntityId>) {
    let summary = sanitize_notification_text(title.unwrap_or("tty7"), NOTIFY_TITLE_MAX);
    let body = sanitize_notification_text(body, NOTIFY_BODY_MAX);

    #[cfg(target_os = "macos")]
    {
        macos_notify::deliver(summary, body, pane.map(|p| p.as_u64()));
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(pane) = pane
            && try_clickable_notification(&summary, &body, pane.as_u64())
        {
            return;
        }

        std::thread::spawn(move || {
            let mut notif = notify_rust::Notification::new();
            notif.summary(&summary).body(&body);
            // Without our own AUMID, the Windows backend falls back to
            // PowerShell's — icon and name included. Only set ours once the
            // shell has indexed a shortcut carrying it: for an AUMID it does
            // not know, `show()` reports success and drops the toast, so the
            // ugly fallback beats the branded one every time we are not sure.
            #[cfg(target_os = "windows")]
            if let Some(app_id) = crate::core::aumid::toast_app_id() {
                notif.app_id(app_id);
            }
            let _ = notif.show();
        });
    }
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

/// What a click on a macOS notification has to carry back: the pane to reveal.
///
/// It rides in the notification's `identifier`, which is the one field the
/// center hands back verbatim on activation without a dictionary round trip.
/// Shape: `tty7-pane-<pid>-<leaf>-<seq>`.
///
/// The pid is what makes a stale identifier fail closed. Notifications outlive
/// the process that sent them, and the center hands a click on one of those to
/// whatever process now owns the bundle id — a relaunched tty7, or a second
/// instance running alongside. A leaf id is a gpui entity id, which a fresh
/// process hands out again in the same order, so without the pid that click
/// would reveal an unrelated pane. The sequence number keeps two notifications
/// for the same pane apart — the center treats a repeated identifier as
/// "replace the earlier one".
#[cfg(any(target_os = "macos", test))]
const NOTIFICATION_ID_PREFIX: &str = "tty7-pane-";

#[cfg(any(target_os = "macos", test))]
fn notification_identifier(leaf_id: u64) -> String {
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("{NOTIFICATION_ID_PREFIX}{pid}-{leaf_id}-{seq}")
}

#[cfg(any(target_os = "macos", test))]
fn leaf_id_in_identifier(identifier: &str) -> Option<u64> {
    let rest = identifier.strip_prefix(NOTIFICATION_ID_PREFIX)?;
    let (pid, rest) = rest.split_once('-')?;
    if pid.parse::<u32>().ok()? != std::process::id() {
        return None;
    }
    let (leaf_id, _seq) = rest.split_once('-')?;
    leaf_id.parse().ok()
}

/// macOS notifications, straight to `NSUserNotificationCenter`.
///
/// This used to go through `mac-notification-sys` with `wait_for_click`, so a
/// click could reveal the pane. The way that crate notices a click is to park
/// the sending thread and, for every notification outstanding, add a repeating
/// 0.5 s timer to the *main* run loop that calls `deliveredNotifications` — a
/// synchronous XPC round trip — to see whether the banner is still there. A
/// banner nobody clicks stays in Notification Center, so its timer never goes
/// away. Sampled with nine outstanding: a fifth of the UI thread inside that
/// XPC, every window juddering, and each agent turn adding one more timer.
///
/// Here the click arrives through the center's delegate, which is what the
/// API is for: no parked thread, no timer, nothing on the main thread until
/// the user actually clicks. The pane rides in the notification's identifier.
///
/// Nothing else may touch the center on this platform. `mac-notification-sys`
/// installs a delegate of its own the first time it sends, the last
/// `setDelegate:` wins, and `notify-rust` is that crate on macOS — so its
/// `show` is never called here, only its `set_application`, which does not
/// install one (that is what names a bare `cargo run` binary to the center).
#[cfg(target_os = "macos")]
#[allow(
    deprecated,
    reason = "UNUserNotificationCenter needs a signed, entitled bundle; NSUserNotification is what a bare binary can use"
)]
mod macos_notify {
    use objc2::rc::{Retained, autoreleasepool};
    use objc2::runtime::ProtocolObject;
    use objc2::{AnyThread, define_class, msg_send};
    use objc2_foundation::{
        NSObject, NSObjectProtocol, NSString, NSUserNotification, NSUserNotificationCenter,
        NSUserNotificationCenterDelegate,
    };

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; `Delegate` has no
        // ivars and no `Drop`.
        #[unsafe(super = NSObject)]
        #[name = "Tty7NotificationDelegate"]
        struct Delegate;

        // SAFETY: `NSObjectProtocol` has no safety requirements.
        unsafe impl NSObjectProtocol for Delegate {}

        // SAFETY: `NSUserNotificationCenterDelegate` has no safety requirements.
        unsafe impl NSUserNotificationCenterDelegate for Delegate {
            /// Runs on the main thread, when the user clicks the banner or the
            /// entry in Notification Center. A channel push, then the clicked
            /// entry is dropped from the center — a one-way message, unlike the
            /// `deliveredNotifications` round trip this module exists to avoid.
            #[unsafe(method(userNotificationCenter:didActivateNotification:))]
            fn did_activate(
                &self,
                center: &NSUserNotificationCenter,
                notification: &NSUserNotification,
            ) {
                if let Some(leaf_id) = notification
                    .identifier()
                    .and_then(|id| super::leaf_id_in_identifier(&id.to_string()))
                {
                    super::reveal_pane(leaf_id);
                }
                center.removeDeliveredNotification(notification);
            }
        }
    );

    fn install_delegate() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // Name the delivering application to the center before it is first
            // touched: the center drops requests from a process with no bundle
            // identity, which is what a bare `cargo run` binary is. This
            // swizzles `-[NSBundle bundleIdentifier]` for the main bundle to
            // `com.github.tty7` when LaunchServices knows that id (a bundled
            // tty7.app, or a machine that has one installed); when it does not,
            // the swizzle's own default, `com.apple.Terminal`, is what the
            // center sees. It can only be called once per process, so there is
            // no second chance to pass a different name.
            let _ = notify_rust::set_application("com.github.tty7");
            // SAFETY: `NSObject`'s `init` takes nothing and returns the object.
            let delegate: Retained<Delegate> = unsafe { msg_send![Delegate::alloc(), init] };
            let center = NSUserNotificationCenter::defaultUserNotificationCenter();
            // SAFETY: `delegate` is a `Delegate`, which conforms to the protocol.
            unsafe { center.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
            // The center holds its delegate unretained. Ours lives as long as
            // the process, so it is simply never released.
            std::mem::forget(delegate);
        });
    }

    /// Hands the notification to the center and returns. `deliverNotification:`
    /// is an XPC message, so it goes out on a short-lived thread rather than
    /// from wherever the OSC sequence was parsed — never the UI thread.
    pub(super) fn deliver(title: String, body: String, leaf_id: Option<u64>) {
        std::thread::spawn(move || {
            autoreleasepool(|_| {
                install_delegate();
                let notification = NSUserNotification::new();
                notification.setTitle(Some(&NSString::from_str(&title)));
                notification.setInformativeText(Some(&NSString::from_str(&body)));
                if let Some(leaf_id) = leaf_id {
                    let id = super::notification_identifier(leaf_id);
                    notification.setIdentifier(Some(&NSString::from_str(&id)));
                }
                NSUserNotificationCenter::defaultUserNotificationCenter()
                    .deliverNotification(&notification);
            });
        });
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
#[cfg(any(target_os = "macos", all(target_os = "windows", not(test))))]
fn reveal_pane(leaf_id: u64) {
    if let Some(tx) = crate::ui::tray::sender() {
        let _ = tx.try_send(crate::ui::tray::TrayAction::RevealPane { leaf_id });
    }
}

#[cfg(not(any(target_os = "macos", all(target_os = "windows", not(test)))))]
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
    fn a_notification_identifier_carries_its_pane_back_out() {
        let id = notification_identifier(42);
        assert_eq!(leaf_id_in_identifier(&id), Some(42));
    }

    #[test]
    fn two_notifications_for_one_pane_do_not_replace_each_other() {
        // The center replaces a delivered notification whose identifier repeats.
        assert_ne!(notification_identifier(7), notification_identifier(7));
    }

    #[test]
    fn a_foreign_identifier_reveals_nothing() {
        let pid = std::process::id();
        for id in [
            String::new(),
            "tty7-pane-".into(),
            "tty7-pane-x-1".into(),
            "tty7-pane-42".into(),
            "other-42-1".into(),
            format!("tty7-pane-{pid}"),
            format!("tty7-pane-{pid}-42"),
            format!("tty7-pane-{pid}-x-1"),
        ] {
            assert_eq!(leaf_id_in_identifier(&id), None, "{id:?}");
        }
    }

    #[test]
    fn a_notification_from_another_process_reveals_nothing() {
        // Notifications outlive the process that sent them, and gpui hands out
        // the same entity ids again in a fresh process. A stale click must not
        // land on whatever pane holds that id now.
        let other_pid = std::process::id().wrapping_add(1);
        let stale = format!("{NOTIFICATION_ID_PREFIX}{other_pid}-42-0");
        assert_eq!(leaf_id_in_identifier(&stale), None);
    }

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

/// Whether the local daemon answers `Version` on a fresh connection right now.
///
/// The one question a silent `Attach` leaves open — is the daemon serving and
/// this socket orphaned, or is nobody serving yet? A daemon answers `Version`
/// before it touches any state, so this is the cheapest thing it can say. One
/// second is the same budget `spawn` gives the same handshake; a healthy
/// daemon answers in microseconds.
fn local_daemon_answers() -> bool {
    use std::io::Write as _;

    let Ok(mut stream) = transport::connect() else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(1)));
    if ClientMsg::Version
        .encode(&mut stream)
        .and_then(|()| stream.flush())
        .is_err()
    {
        return false;
    }
    matches!(DaemonMsg::read(&mut stream), Ok(DaemonMsg::Version(_)))
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

/// Issue #857 asked whether a read boundary can leave the grid different from
/// the one the same bytes parse into in one go — a Pi response streamed as
/// synchronized differential frames (cursor-up, erase-line, rewrite) with
/// OSC 133 marks and CJK text. The reader hands the emulator whatever the
/// socket delivers, so the grid has to come out the same for every split.
#[cfg(test)]
mod chunking_tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions as _;
    use alacritty_terminal::index::{Column, Line};

    const COLS: usize = 24;
    const ROWS: usize = 8;

    fn parse(chunks: &[&[u8]]) -> Vec<String> {
        let config = terminal_config_from_user(&crate::core::config::Config::default());
        let mut term = Term::new(config, &TermSize::new(COLS, ROWS), VoidListener);
        let mut processor: ansi::Processor = ansi::Processor::new();
        for chunk in chunks {
            processor.advance(&mut term, chunk);
        }
        assert!(
            processor.sync_timeout().sync_timeout().is_none(),
            "every frame closes its synchronized update"
        );
        let grid = term.grid();
        let mut rows: Vec<String> = (0..grid.screen_lines() as i32)
            .map(|line| {
                (0..COLS)
                    .map(|col| {
                        let cell = &grid[Line(line)][Column(col)];
                        let mut s = format!("{}{:?}", cell.c, cell.flags);
                        for z in cell.zerowidth().unwrap_or(&[]) {
                            s.push(*z);
                        }
                        s
                    })
                    .collect()
            })
            .collect();
        rows.push(format!("{:?}", grid.cursor.point));
        rows
    }

    /// Three frames of a response streaming in, each redrawing the lines it
    /// touched the way Pi's main-screen renderer does.
    fn stream() -> Vec<u8> {
        let mark = "\x1b]133;B\x07\x1b]133;C\x07";
        let frames = [
            format!("\x1b[?2026h\r\x1b[2K例{mark}Ex\x1b[?2026l"),
            format!(
                "\x1b[?2026h\r\x1b[2K例子：Example sentence.\r\n\r\n\x1b[2K## 资源{mark}N\x1b[?2026l"
            ),
            format!(
                "\x1b[?2026h\x1b[2A\r\x1b[2K例子：Example.❤️\r\n\r\n\x1b[2K## 资源\r\n\
                 \x1b[2K{mark}下一步：Next step.\x1b[?2026l"
            ),
        ];
        frames.concat().into_bytes()
    }

    #[test]
    fn a_streamed_response_parses_the_same_at_every_split() {
        let bytes = stream();
        let whole = parse(&[&bytes]);
        assert!(
            whole.iter().all(|row| !row.contains("33;")),
            "no mark reaches the grid as text"
        );
        for cut in 0..=bytes.len() {
            assert_eq!(
                parse(&[&bytes[..cut], &bytes[cut..]]),
                whole,
                "split at byte {cut}"
            );
        }
        let bytewise: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(parse(&bytewise), whole, "a byte at a time");
    }

    /// A batch several lock-holds long, colored and multi-byte so slices end
    /// mid-sequence and mid-character.
    fn long_batch() -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut n = 0;
        while bytes.len() < 3 * MAX_LOCKED_FEED + 777 {
            bytes.extend_from_slice(format!("\x1b[3{}m行{n}\x1b[0m\r\n", n % 8).as_bytes());
            n += 1;
        }
        bytes
    }

    fn fresh() -> Term<VoidListener> {
        let config = terminal_config_from_user(&crate::core::config::Config::default());
        Term::new(config, &TermSize::new(COLS, ROWS), VoidListener)
    }

    /// Where the cursor stood at each cut, and the screen after the whole batch.
    type Seen = (Vec<(i32, usize)>, Vec<String>);

    fn screen(term: &Term<VoidListener>) -> Vec<String> {
        let grid = term.grid();
        (0..ROWS as i32)
            .map(|line| {
                (0..COLS)
                    .map(|col| grid[Line(line)][Column(col)].c)
                    .collect()
            })
            .collect()
    }

    fn cursor(term: &Term<VoidListener>) -> (i32, usize) {
        let point = term.grid().cursor.point;
        (point.line.0, point.column.0)
    }

    /// The reader lets go of the grid every `MAX_LOCKED_FEED` bytes, and not
    /// one of those slices may show: the screen, and the state each cut acts
    /// on, come out as if the batch had been parsed in one hold — including a
    /// cut that lands exactly on a slice boundary.
    #[test]
    fn a_batch_fed_in_slices_parses_as_one() {
        let bytes = long_batch();
        let offsets = [
            0,
            5,
            MAX_LOCKED_FEED,
            MAX_LOCKED_FEED + 1,
            2 * MAX_LOCKED_FEED + 9,
            bytes.len(),
        ];

        let whole: Seen = {
            let mut term = fresh();
            let mut processor: ansi::Processor = ansi::Processor::new();
            let mut at = 0;
            let mut stops = Vec::new();
            for &off in &offsets {
                processor.advance(&mut term, &bytes[at..off]);
                at = off;
                stops.push(cursor(&term));
            }
            (stops, screen(&term))
        };

        let term = FairMutex::new(fresh());
        let mut processor: ansi::Processor = ansi::Processor::new();
        let cuts = offsets
            .iter()
            .map(|&off| (off, ReaderCut::Command(CommandMark::Finished)))
            .collect();
        let mut stops = Vec::new();
        let fed = feed_grid(
            &term,
            &mut processor,
            &bytes,
            cuts,
            &AtomicBool::new(false),
            |term, _| stops.push(cursor(term)),
        );
        assert!(fed.is_some());
        assert_eq!((stops, screen(&term.lock())), whole);
    }

    /// A reader retired while it waited for the grid writes nothing into it:
    /// the grid belongs to its successor now.
    #[test]
    fn a_retired_reader_leaves_the_grid_alone() {
        let term = FairMutex::new(fresh());
        let before = screen(&term.lock());
        let mut processor: ansi::Processor = ansi::Processor::new();
        let fed = feed_grid(
            &term,
            &mut processor,
            b"stale",
            Vec::new(),
            &AtomicBool::new(true),
            |_, _| {},
        );
        assert!(fed.is_none());
        assert_eq!(screen(&term.lock()), before);
    }
}

/// A point in a batch of pty output where the reader stops advancing the
/// emulator to act on the state the bytes before it left behind.
enum ReaderCut {
    Parked(CursorCut),
    Command(CommandMark),
}

/// Adds the batch's command marks to `cuts`, keeping them in stream order
/// alongside whatever cuts are already there.
fn command_cuts(tok: &mut OscTokenizer, bytes: &[u8], cuts: &mut Vec<(usize, ReaderCut)>) {
    let before = cuts.len();
    tok.feed_at(bytes, |off, payload| {
        if let Some(mark) = CommandMark::parse(payload) {
            cuts.push((off, ReaderCut::Command(mark)));
        }
    });
    if before > 0 && cuts.len() > before {
        // Stable, so a mark that ends where a cursor show does keeps its place
        // after it.
        cuts.sort_by_key(|(off, _)| *off);
    }
}

/// How much output the reader parses per hold of the grid lock. Every UI-thread
/// handler that touches the grid — a scroll, a selection drag, a keystroke —
/// queues on this lock, and one UI thread serves every window, so a hold is how
/// long all of them can stall. Alacritty's own reader lets go at the same size
/// (`MAX_LOCKED_READ`).
const MAX_LOCKED_FEED: usize = 64 * 1024;

/// Parses `bytes` into the emulator, stopping at each of `cuts` for `on_cut` to
/// act on the state the bytes before it left behind — and letting go of the
/// grid every [`MAX_LOCKED_FEED`] bytes, so a waiting handler gets its turn
/// between slices rather than after the whole batch. A relink replays up to
/// 8 MB in one frame; parsed in one hold, that froze every window for as long
/// as it took.
///
/// The lock is fair, so re-taking it queues behind whoever arrived meanwhile.
/// vte keeps its own state across `advance` calls, so where a slice ends — mid
/// escape sequence, mid UTF-8 — makes no difference to what is parsed.
///
/// Returns how long it spent waiting for the lock, or `None` if `quit` was
/// raised while it waited: a retired reader must not write into a grid its
/// successor now owns.
fn feed_grid<T: EventListener>(
    term: &FairMutex<Term<T>>,
    processor: &mut ansi::Processor,
    bytes: &[u8],
    cuts: Vec<(usize, ReaderCut)>,
    quit: &AtomicBool,
    mut on_cut: impl FnMut(&mut Term<T>, ReaderCut),
) -> Option<std::time::Duration> {
    let mut waited = std::time::Duration::ZERO;
    let mut lock = || {
        let t = std::time::Instant::now();
        let grid = term.lock();
        waited += t.elapsed();
        (!quit.load(Ordering::SeqCst)).then_some(grid)
    };
    let mut grid = lock()?;
    let mut held = 0usize;
    let mut at = 0usize;
    let mut cuts = cuts.into_iter();
    let mut next = cuts.next();
    loop {
        let stop = next.as_ref().map_or(bytes.len(), |(off, _)| *off);
        while at < stop {
            if held == MAX_LOCKED_FEED {
                drop(grid);
                grid = lock()?;
                held = 0;
            }
            let end = stop.min(at + MAX_LOCKED_FEED - held);
            processor.advance(&mut *grid, &bytes[at..end]);
            held += end - at;
            at = end;
        }
        let Some((_, cut)) = next else { break };
        on_cut(&mut grid, cut);
        next = cuts.next();
    }
    drop(grid);
    Some(waited)
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

/// What a re-attach's replay actually leaves in the grid.
///
/// Switching workspaces tears every pane down and attaches to the same daemon
/// pane again (#711), so the whole of a pane's screen has to survive one trip
/// through the replay — several ring segments, each preceded by the geometry it
/// was written at, and a snapshot frame far larger than one socket read. These
/// drive that over a real socket pair rather than a mock, because the loss
/// being hunted is between the wire and the grid; and they are not `cfg(unix)`
/// like their neighbours because nothing about the path is.
#[cfg(test)]
mod replay_tests {
    use super::*;
    use crate::daemon::protocol::DaemonMsg;
    use crate::daemon::transport::Stream;

    pub(super) fn socket_pair() -> (Stream, Stream) {
        #[cfg(unix)]
        {
            std::os::unix::net::UnixStream::pair().unwrap()
        }
        #[cfg(windows)]
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let client_side = std::net::TcpStream::connect(addr).unwrap();
            let (daemon_side, _) = listener.accept().unwrap();
            (client_side, daemon_side)
        }
    }

    fn ws(cols: u16, rows: u16) -> WinSize {
        WinSize {
            cols,
            rows,
            cell_w: 8,
            cell_h: 17,
        }
    }

    /// Everything the grid holds, scrollback included, one row per line.
    fn all_text(term: &RemoteTerminal) -> String {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line};
        let t = term.term.lock();
        let grid = t.grid();
        let mut out = String::new();
        let top = -(grid.history_size() as i32);
        for line in top..grid.screen_lines() as i32 {
            let row = &grid[Line(line)];
            let mut text = String::new();
            for col in 0..grid.columns() {
                text.push(row[Column(col)].c);
            }
            let text = text.trim_end();
            if !text.is_empty() {
                out.push_str(text);
                out.push('\n');
            }
        }
        out
    }

    /// The visible screen alone.
    fn screen_text(term: &RemoteTerminal) -> String {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line};
        let t = term.term.lock();
        let grid = t.grid();
        let mut out = String::new();
        for line in 0..grid.screen_lines() as i32 {
            let row = &grid[Line(line)];
            let mut text = String::new();
            for col in 0..grid.columns() {
                text.push(row[Column(col)].c);
            }
            let text = text.trim_end();
            if !text.is_empty() {
                out.push_str(text);
                out.push('\n');
            }
        }
        out
    }

    /// The grid's width. A grid is born at the size its `RemoteTerminal` was
    /// built with and only ever leaves it on a `DaemonMsg::Size`, so this is
    /// what says which geometry frames were applied.
    fn columns(term: &RemoteTerminal) -> usize {
        use alacritty_terminal::grid::Dimensions as _;
        term.term.lock().grid().columns()
    }

    /// Waits for the reader thread to have applied everything named, then
    /// returns the whole grid either way so a failure can print what landed.
    fn settled(term: &RemoteTerminal, needles: &[&str]) -> String {
        for _ in 0..600 {
            let text = all_text(term);
            if needles.iter().all(|n| text.contains(n)) {
                return text;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        all_text(term)
    }

    /// The shape a re-attach takes: the client's grid is born at the hardcoded
    /// attach size, and the daemon replays every ring segment at the geometry
    /// it was recorded at.
    #[test]
    fn every_replayed_segment_reaches_the_grid() {
        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon) = socket_pair();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Size(ws(80, 24)).encode(&mut daemon).unwrap();
        DaemonMsg::Snapshot(b"BIRTH-BANNER\r\n".to_vec())
            .encode(&mut daemon)
            .unwrap();
        DaemonMsg::Size(ws(120, 40)).encode(&mut daemon).unwrap();
        DaemonMsg::Snapshot(b"SECOND-SEGMENT\r\n".to_vec())
            .encode(&mut daemon)
            .unwrap();
        DaemonMsg::Size(ws(120, 40)).encode(&mut daemon).unwrap();
        DaemonMsg::Snapshot(b"THIRD-SEGMENT\r\n".to_vec())
            .encode(&mut daemon)
            .unwrap();

        let text = settled(&term, &["BIRTH-BANNER", "SECOND-SEGMENT", "THIRD-SEGMENT"]);
        assert!(text.contains("BIRTH-BANNER"), "grid held:\n{text}");
        assert!(text.contains("SECOND-SEGMENT"), "grid held:\n{text}");
        assert!(text.contains("THIRD-SEGMENT"), "grid held:\n{text}");
        // Each segment's geometry too, or this would pass on a replay that
        // dropped every `Size`. The unix-gated
        // `segmented_ring_replay_reproduces_live_rendering` asserts that half
        // already; nothing did on the platforms this module exists for.
        assert_eq!(
            columns(&term),
            120,
            "the geometry each segment was recorded at never reached the grid, \
             so it is still at the attach size"
        );
        drop(daemon);
    }

    /// A real pane's ring is megabytes; one `Snapshot` frame is far larger than
    /// the reader's 256 KiB read buffer, so the frame is assembled across many
    /// reads — several of which time out at `QUIT_POLL` while the sender is
    /// still pushing.
    #[test]
    fn a_snapshot_larger_than_one_read_still_lands_whole() {
        crate::core::config::pin_test_config_dir();
        let (client_side, daemon) = socket_pair();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        let mut bulk = Vec::new();
        bulk.extend_from_slice(b"HEAD-OF-THE-RING\r\n");
        for i in 0..40_000 {
            bulk.extend_from_slice(format!("line {i} of the pane's history\r\n").as_bytes());
        }
        bulk.extend_from_slice(b"TAIL-OF-THE-RING\r\n");

        let feeder = std::thread::spawn(move || {
            let mut daemon = daemon;
            DaemonMsg::Size(ws(120, 40)).encode(&mut daemon).unwrap();
            DaemonMsg::Snapshot(bulk).encode(&mut daemon).unwrap();
            DaemonMsg::Size(ws(120, 40)).encode(&mut daemon).unwrap();
            DaemonMsg::Snapshot(b"AFTER-THE-BULK\r\n".to_vec())
                .encode(&mut daemon)
                .unwrap();
            daemon
        });

        let text = settled(&term, &["AFTER-THE-BULK"]);
        assert!(
            text.contains("AFTER-THE-BULK"),
            "the frame after a multi-megabyte snapshot never arrived; grid tail:\n{}",
            screen_text(&term)
        );
        assert!(
            text.contains("TAIL-OF-THE-RING"),
            "the snapshot itself was truncated"
        );
        drop(feeder.join().unwrap());
    }

    /// The view resizes to its real geometry the first time it paints, which
    /// happens while the replay is still arriving. Against a resize-echoing
    /// daemon the grid must not reflow when the request goes out — only when
    /// the daemon echoes the `Size` back, which is the stream position where
    /// the bytes stop being old-width — and neither step may cost the
    /// replayed screen.
    ///
    /// The two geometries are deliberately different: at identical dimensions
    /// a reflow and a deferred reflow look the same, so the invariant would be
    /// unobservable.
    #[test]
    fn a_resize_racing_the_replay_keeps_the_replayed_screen() {
        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon) = socket_pair();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        term.route = echoing_route();

        DaemonMsg::Size(ws(120, 40)).encode(&mut daemon).unwrap();
        DaemonMsg::Snapshot(b"REPLAYED-SCREEN\r\n".to_vec())
            .encode(&mut daemon)
            .unwrap();
        let text = settled(&term, &["REPLAYED-SCREEN"]);
        assert!(text.contains("REPLAYED-SCREEN"), "grid held:\n{text}");

        assert_eq!(columns(&term), 120, "the replay set the grid's geometry");

        // First paint: the pane is 100x30 on screen, not the 120x40 the ring
        // was recorded at. The request goes down the link and the grid stays
        // where the replay left it.
        term.resize(TermSize::new(100, 30), 8, 17);
        assert_eq!(
            columns(&term),
            120,
            "the grid reflowed at request time instead of waiting for the echo"
        );
        let _ = daemon.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        match ClientMsg::read(&mut daemon) {
            Ok(ClientMsg::Resize(size)) => assert_eq!((size.cols, size.rows), (100, 30)),
            other => panic!("the resize never reached the daemon: {other:?}"),
        }

        // The echo is the stream position the reflow belongs at.
        DaemonMsg::Size(ws(100, 30)).encode(&mut daemon).unwrap();
        for _ in 0..600 {
            if columns(&term) == 100 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let text = all_text(&term);
        assert_eq!(columns(&term), 100, "the echo never reflowed the grid");
        assert!(
            text.contains("REPLAYED-SCREEN"),
            "the first paint's resize cost the replay; grid held:\n{text}"
        );
        drop(daemon);
    }

    /// The attach handshake reads off the same socket the reader will, far
    /// enough to tell an `Error` frame from a replay, and hands what it read
    /// on as the reader's starting buffer. A whole replay can already be
    /// sitting in the socket when it looks, so the prefix it takes is several
    /// frames wide and the split lands mid-frame — every byte of it has to
    /// reach the grid.
    #[test]
    fn the_attach_handshakes_prefix_carries_the_replay_it_swallowed() {
        crate::core::config::pin_test_config_dir();
        let (mut client_side, mut daemon) = socket_pair();

        // The whole replay before the client looks: this is the daemon that
        // answered instantly, which is the daemon a local attach meets.
        DaemonMsg::Size(ws(80, 24)).encode(&mut daemon).unwrap();
        DaemonMsg::Snapshot(b"BIRTH-BANNER\r\n".to_vec())
            .encode(&mut daemon)
            .unwrap();
        DaemonMsg::Size(ws(120, 40)).encode(&mut daemon).unwrap();
        let mut bulk = Vec::new();
        for i in 0..400 {
            bulk.extend_from_slice(format!("scrollback row {i}\r\n").as_bytes());
        }
        bulk.extend_from_slice(b"LAST-ROW-OF-THE-RING\r\n");
        DaemonMsg::Snapshot(bulk).encode(&mut daemon).unwrap();

        let buffered =
            attach_reply_prefix(&mut client_side, 1, std::time::Duration::from_secs(2)).unwrap();
        assert!(
            !buffered.is_empty(),
            "the handshake read nothing, so it proves nothing"
        );
        let term = RemoteTerminal::from_stream_with(
            client_side,
            TermSize::new(80, 24),
            buffered,
            PtySource::Raw,
        )
        .unwrap();

        let text = settled(&term, &["BIRTH-BANNER", "LAST-ROW-OF-THE-RING"]);
        assert!(text.contains("BIRTH-BANNER"), "grid held:\n{text}");
        assert!(
            text.contains("LAST-ROW-OF-THE-RING"),
            "the replay the handshake swallowed never reached the grid"
        );
        drop(daemon);
    }

    /// Issue #711: what a prompt report costs a replay that ended inside the
    /// alternate screen.
    ///
    /// A re-attach replays the pane's ring and then the pane's *state*, and
    /// the state ends with the shell's prompt status. `active && at_prompt`
    /// makes the reader scrub the TUI modes it finds in the grid, alternate
    /// screen first — see `prompt_report_scrubs_stale_tui_modes`, which is the
    /// case that is meant to reach it: a program that died without its
    /// `?1049l` leaves the grid stranded on a screen nothing owns, and the
    /// shell's next prompt is what proves it stale.
    ///
    /// On a replay the alternate screen the scrub finds is the one the ring
    /// just rebuilt, and `?1049l` does not undo it — it swaps it away. The
    /// grid is left holding the primary screen underneath: the banner and the
    /// command line the pane was born with, which is exactly what #711's
    /// reporter photographed. The client cannot tell the two apart from the
    /// frame, so the daemon decides — it declines to claim a prompt while a
    /// foreground command owns the pty (`daemon::pane::replayed_at_prompt`).
    /// Both halves of that contract are pinned here, from the side that pays
    /// for it.
    #[test]
    fn a_replayed_prompt_report_decides_whether_the_alternate_screen_survives() {
        crate::core::config::pin_test_config_dir();

        // A re-attach, as the daemon writes one: the shell's banner on the
        // primary screen, the agent's alternate screen drawn over it, then the
        // pane's stored state. The trailing `Output` is a barrier — frames are
        // applied in order, so a grid that holds it has already been through
        // the prompt report, and neither half can pass on a race.
        let replayed_pane = |at_prompt: bool| {
            let (client_side, mut daemon) = socket_pair();
            let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
            DaemonMsg::Size(ws(80, 24)).encode(&mut daemon).unwrap();
            DaemonMsg::Snapshot(
                b"BIRTH-BANNER\r\nMac:Accounts joe$ claude --resume --fork-session\r\n".to_vec(),
            )
            .encode(&mut daemon)
            .unwrap();
            DaemonMsg::Snapshot(b"\x1b[?1049h\x1b[2J\x1b[HAGENT-SCREEN\r\n".to_vec())
                .encode(&mut daemon)
                .unwrap();
            DaemonMsg::Prompt {
                active: true,
                at_prompt,
                last_exit: Some(0),
            }
            .encode(&mut daemon)
            .unwrap();
            DaemonMsg::Output(b"REPORT-APPLIED\r\n".to_vec())
                .encode(&mut daemon)
                .unwrap();
            let text = settled(&term, &["REPORT-APPLIED"]);
            drop(daemon);
            text
        };

        // A program owns the pane, so the replay reports no prompt and the
        // screen the ring rebuilt is the screen the pane shows.
        let text = replayed_pane(false);
        assert!(
            text.contains("REPORT-APPLIED"),
            "the barrier never arrived, so nothing below is tested; grid held:\n{text}"
        );
        assert!(
            text.contains("AGENT-SCREEN"),
            "a replay that does not claim a prompt must leave the alternate screen it \
             rebuilt alone; grid held:\n{text}"
        );

        // The same replay under the stale claim: the scrub swaps the agent's
        // screen away, and what the pane comes back showing is the banner it
        // was born with. That is #711.
        let text = replayed_pane(true);
        assert!(
            !text.contains("AGENT-SCREEN"),
            "a prompt report still has to scrub a stranded alternate screen, or the daemon's \
             gate is load-bearing for nothing; grid held:\n{text}"
        );
        assert!(
            text.contains("BIRTH-BANNER"),
            "what the scrub leaves is the primary screen underneath; grid held:\n{text}"
        );
    }

    // ---- The switch, end to end, with a real daemon pane ----------------
    //
    // Everything above drives the client half against a scripted daemon. This
    // drives the real one: a live `DaemonPane` over a real pty, in this
    // process, with its `DaemonMsg` stream forwarded onto a socket pair the
    // way `spawn_writer` forwards it, so an attach here is the same attach a
    // re-attach makes. It is the switch (#711) minus gpui: attach, resize to
    // the geometry the window actually has, produce output, drop the client,
    // produce more, attach again — and read the grid the second client ends up
    // with.

    /// A client hung off `pane`, the way `stream_pane_with_attach` hangs one
    /// off it: subscribe, forward every queued message onto the wire, and read
    /// the client's own frames back the way `run_stream` does — epoch guard
    /// included, so a displaced client's resize is dropped here exactly as the
    /// daemon drops it.
    ///
    /// The route is a resize-echoing one, which is what the local daemon is:
    /// the grid must not reflow until the `Size` the daemon echoes back.
    fn attach_client(
        pane: &std::sync::Arc<tty7_core::daemon::pane::DaemonPane>,
    ) -> (u64, RemoteTerminal, std::thread::JoinHandle<()>) {
        let (client_side, daemon_side) = socket_pair();
        let mut daemon_write = daemon_side.try_clone().expect("clone the daemon half");
        let (tx, rx) = std::sync::mpsc::channel::<DaemonMsg>();
        let epoch = pane.attach(tx);
        let gate = pane.gate();
        let forward = std::thread::spawn(move || {
            while let Ok(msg) = rx.recv() {
                let drained = match &msg {
                    DaemonMsg::Output(b) | DaemonMsg::Image(b) => b.len(),
                    _ => 0,
                };
                let ok = msg.encode(&mut daemon_write).is_ok();
                if drained > 0 {
                    gate.sub(drained);
                }
                if !ok {
                    break;
                }
            }
        });
        {
            let pane = pane.clone();
            let mut daemon_read = daemon_side;
            std::thread::spawn(move || {
                use std::io::Read as _;
                let mut pending: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 65536];
                loop {
                    while let Ok(Some((kind, payload))) =
                        crate::daemon::protocol::take_frame(&mut pending)
                    {
                        match ClientMsg::from_frame(kind, payload) {
                            Ok(ClientMsg::Input(bytes)) if pane.controls(epoch) => {
                                pane.write_input(&bytes)
                            }
                            Ok(ClientMsg::Resize(size)) if pane.controls(epoch) => {
                                pane.resize(size)
                            }
                            Ok(_) => {}
                            Err(_) => return,
                        }
                    }
                    match daemon_read.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => pending.extend_from_slice(&chunk[..n]),
                    }
                }
            });
        }
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24))
            .expect("a client over the pair");
        term.route = echoing_route();
        (epoch, term, forward)
    }

    /// A route whose daemon echoes `Size` when it applies a resize — which is
    /// what `PaneRoute::Local` is against a current daemon, and what the
    /// harness above implements.
    ///
    /// Built the way the neighbouring resize tests build one, rather than
    /// through a `PaneWorkspace`: routing a workspace is not what any of these
    /// cover, and going that way would have a change to `NativeSshSpec`'s
    /// serde shape fail them on an `unwrap` that has nothing to do with
    /// replay.
    fn echoing_route() -> PaneRoute {
        PaneRoute::Remote {
            header: Box::new(crate::daemon::router::RouteHeader::wsl("Ubuntu-22.04")),
            resize_echo: true,
        }
    }

    fn wait_for(term: &RemoteTerminal, needle: &str) -> bool {
        for _ in 0..600 {
            if all_text(term).contains(needle) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        false
    }

    /// Types `line` at the pane until the grid echoes `needle` back, and
    /// answers whether it ever did.
    ///
    /// One write is not enough for the *first* command a fresh pane is given.
    /// `spawn` returning means the pty exists, not that the shell behind it
    /// has started, printed a prompt, or begun reading — and on Windows the
    /// ConPTY has not necessarily connected the child to its input pipe yet,
    /// so bytes typed into that window reach nobody at all. On a loaded runner
    /// the window is wide: both cases showed up on Windows CI as a grid that
    /// was still completely empty after 15s, so not even a prompt had been
    /// printed, let alone an echo.
    ///
    /// Retyping is safe for everything asserted here: a line that did land and
    /// was merely slow simply runs twice, and every assertion is a `contains`.
    /// The later commands in these tests keep their single `write_input` —
    /// by then the shell has echoed once, which is proof it is reading.
    fn type_until_echoed(
        pane: &tty7_core::daemon::pane::DaemonPane,
        line: &[u8],
        term: &RemoteTerminal,
        needle: &str,
    ) -> bool {
        for _ in 0..15 {
            pane.write_input(line);
            for _ in 0..80 {
                if all_text(term).contains(needle) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
        false
    }

    /// Switching workspaces drops every pane and attaches to the same daemon
    /// panes again. Whatever the pane put on screen while the window was
    /// elsewhere — and whatever it had already — has to come back with it.
    #[test]
    fn a_pane_re_attached_after_a_switch_gets_its_screen_back() {
        crate::core::config::pin_test_config_dir();
        let pane = tty7_core::daemon::pane::DaemonPane::spawn(
            7711,
            std::env::current_dir().ok(),
            ws(80, 24),
            None,
            None,
            None,
            None,
            false,
            || {},
        )
        .expect("a pty-backed pane");

        // The window this pane is shown in, attaching for the first time.
        let (epoch, mut first, forward) = attach_client(&pane);
        // What the first paint does, from the same side it does it on: the
        // pane is not 80x24 on screen, so the real geometry goes down the link
        // and the grid waits for the daemon to echo it back.
        first.resize(TermSize::new(120, 40), 8, 17);
        assert!(
            type_until_echoed(
                &pane,
                b"echo BEFORE-THE-SWITCH\r",
                &first,
                "BEFORE-THE-SWITCH"
            ),
            "the pane never echoed the first command; grid held:\n{}",
            all_text(&first)
        );

        // Switching away: the view is dropped, which closes the link, and the
        // daemon gives up the seat.
        drop(first);
        pane.detach(epoch);
        drop(forward);

        // The pane keeps working while the window is showing another workspace.
        pane.write_input(b"echo DURING-THE-SWITCH\r");
        std::thread::sleep(std::time::Duration::from_millis(600));

        // Switching back: a brand new view, attaching at the hardcoded size.
        //
        // It deliberately never resizes. Resizing is the workaround #711's
        // reporter found — it makes the daemon `TIOCSWINSZ` the pty and the
        // *child* repaint, which would put the screen back whether or not the
        // replay ever arrived, and would also feed the grid a `Size` echo that
        // `resize_state` sends unconditionally. Everything asserted below has
        // to come from the replay itself.
        let (_epoch, second, forward) = attach_client(&pane);
        let landed = wait_for(&second, "DURING-THE-SWITCH");
        let text = all_text(&second);
        let width = columns(&second);
        pane.kill();
        drop(forward);

        assert!(
            landed,
            "the re-attached pane never got the output produced while it was hidden; \
             grid held:\n{text}"
        );
        assert!(
            text.contains("BEFORE-THE-SWITCH"),
            "the re-attached pane lost the screen it had before the switch; grid held:\n{text}"
        );
        // Nothing resized this client, so 120 can only have come from the
        // `Size` frame the replay sends ahead of the segment recorded at it.
        assert_eq!(
            width, 120,
            "the replay's geometry never reached the grid, so it is still at the attach size"
        );
    }

    /// A switch can rebuild a window twice — a second hydration lands while the
    /// first one's panes are still draining their replay — so the same pane is
    /// attached to twice in quick succession and the window keeps the later
    /// view. That view must hold the screen, and it must still be the one the
    /// daemon obeys: the displaced attach's teardown must not take the seat
    /// with it.
    ///
    /// The needle is weaker here than in the switch test above, and knowingly
    /// so: the pane is attached to throughout, and a ConPTY that owns the whole
    /// viewport reprints what is on screen as ordinary live output, so
    /// `RACED-OUTPUT` can reach the second view without the replay. What only
    /// the replay can supply is the geometry — nothing resizes this client —
    /// and what only the seat can supply is `AFTER-THE-RACE`. Those two are the
    /// assertions that discriminate.
    #[test]
    fn the_later_of_two_racing_attaches_keeps_the_screen_and_the_seat() {
        crate::core::config::pin_test_config_dir();
        let pane = tty7_core::daemon::pane::DaemonPane::spawn(
            7712,
            std::env::current_dir().ok(),
            ws(80, 24),
            None,
            None,
            None,
            None,
            false,
            || {},
        )
        .expect("a pty-backed pane");

        let (first_epoch, mut first, first_forward) = attach_client(&pane);
        first.resize(TermSize::new(120, 40), 8, 17);
        assert!(
            type_until_echoed(&pane, b"echo RACED-OUTPUT\r", &first, "RACED-OUTPUT"),
            "the pane never echoed; grid held:\n{}",
            all_text(&first)
        );

        // The second rebuild attaches before the first one's view is dropped.
        let (second_epoch, second, second_forward) = attach_client(&pane);
        drop(first);
        drop(first_forward);
        // The displaced connection detaches on its way out, epoch-guarded.
        pane.detach(first_epoch);

        // Never resized, for the reason the switch test is not: a resize would
        // repaint the pane from the child and echo a `Size` back, and both are
        // exactly what must not be allowed to stand in for the replay.
        let landed = wait_for(&second, "RACED-OUTPUT");
        let width = columns(&second);
        // Two halves of "the seat survived", and only one of them can catch a
        // detach that took it. `controls` answers from `subscriber_epoch`
        // alone, and `DaemonPane::detach` clears `subscriber` without ever
        // touching that counter — so it says the pane would obey this view's
        // input and resizes, but it would keep saying so with the epoch guard
        // stripped out of `detach`. `live` is what notices that: a detach that
        // dropped the wrong subscriber silences the surviving view.
        let controls = pane.controls(second_epoch);
        pane.write_input(b"echo AFTER-THE-RACE\r");
        let live = wait_for(&second, "AFTER-THE-RACE");
        let text = all_text(&second);
        pane.kill();
        drop(second_forward);

        assert!(
            landed,
            "the second attach's replay was lost; grid held:\n{text}"
        );
        assert_eq!(
            width, 120,
            "the second attach's replay carried no geometry, so the grid is still at the \
             attach size"
        );
        assert!(
            controls,
            "the pane no longer answers to the surviving view, so nothing it \
             types or resizes would reach the pty"
        );
        assert!(
            live,
            "the displaced attach's detach took the live seat: the surviving \
             view stopped receiving output"
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    use super::replay_tests::socket_pair as tcp_pair;

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
            term.adopt_relink(
                new_client,
                Vec::new(),
                &PaneRoute::Local,
                TermSize::new(80, 24),
                8,
                16,
            )
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

    /// What the daemon replays into a pane restored from a stored screen, in
    /// the frames and the order it sends them: the dead pane's screen, then the
    /// preamble, then the new shell's own first output.
    fn replay_restore_into(daemon: &mut std::net::TcpStream, old_screen: &[u8], shell: &[u8]) {
        let size = WinSize {
            cols: 40,
            rows: 10,
            cell_w: 8,
            cell_h: 17,
        };
        DaemonMsg::Size(size).encode(daemon).unwrap();
        DaemonMsg::Snapshot(old_screen.to_vec())
            .encode(daemon)
            .unwrap();
        DaemonMsg::Size(size).encode(daemon).unwrap();
        DaemonMsg::Snapshot(crate::daemon::pane::restore_preamble(Some(
            "the shell below is new",
        )))
        .encode(daemon)
        .unwrap();
        DaemonMsg::Output(shell.to_vec()).encode(daemon).unwrap();
    }

    fn row(term: &RemoteTerminal, line: i32) -> String {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line};
        let term = term.term.lock();
        let grid = term.grid();
        (0..grid.columns())
            .map(|c| grid[Line(line)][Column(c)].c)
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// A restored screen must not be sitting in the viewport when the new
    /// shell's ConPTY starts drawing on it.
    ///
    /// conhost addresses its own screen buffer absolutely — PSReadLine repaints
    /// the line being typed with `ESC[6;20H` and conhost frames it the same way
    /// — and that buffer starts blank with the cursor at the top-left. Restored
    /// output is output conhost never produced: left on screen it shifts every
    /// row conhost names, so the first keystroke repaints the input line on top
    /// of the old text instead of at the prompt. The restored screen belongs in
    /// scrollback, where it survives without claiming a row.
    #[test]
    fn a_restored_screen_leaves_the_new_shell_the_viewport_conpty_thinks_it_has() {
        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = tcp_pair();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(40, 10)).unwrap();

        // Five lines of the dead pane, then the shell painting its prompt the
        // way conhost does: at row 1 of a buffer it believes is blank.
        replay_restore_into(
            &mut daemon_side,
            b"line one\r\nline two\r\nline three\r\nline four\r\nline five\r\n",
            b"\x1b[?25l\x1b[1;1HPS C:\\> \x1b[?25h",
        );

        let mut top = String::new();
        for _ in 0..400 {
            top = row(&term, 0);
            if top.starts_with("PS C:") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            top, "PS C:\\>",
            "the shell's prompt paints where conhost put it"
        );

        for line in 1..10 {
            assert_eq!(
                row(&term, line),
                "",
                "row {line} still holds restored output, so conhost and the client \
                 disagree about which row is which: the next repaint of the input \
                 line lands on the old screen instead of at the prompt"
            );
        }

        // Kept, not erased: `ESC[2J` on the primary screen scrolls the viewport
        // into history, so the screen the daemon restored is one scroll away.
        let depth = {
            use alacritty_terminal::grid::Dimensions as _;
            term.term.lock().grid().history_size() as i32
        };
        let history: Vec<String> = (-depth..0).map(|line| row(&term, line)).collect();
        for wanted in [
            "line one",
            "line two",
            "line three",
            "line four",
            "line five",
        ] {
            assert!(
                history.iter().any(|row| row == wanted),
                "{wanted:?} is not in the scrollback; the restored screen was erased \
                 rather than scrolled away. History holds {history:?}"
            );
        }
    }
}

/// Ungated on purpose: what a workspace can build a route out of is the same
/// on every platform, and so is the name the refusal carries.
#[cfg(test)]
mod route_header_tests {
    use super::*;
    use crate::core::session::{RemoteTarget, WorkspaceId};

    fn unroutable(target: RemoteTarget, label: Option<&str>) -> PaneWorkspace {
        PaneWorkspace {
            workspace: WorkspaceId::new(),
            target,
            spec: None,
            label: label.map(str::to_string),
            resize_echo: false,
        }
    }

    /// A deleted profile is what empties `spec`, and the refusal built here is
    /// what the pending pane prints verbatim under "could not reach
    /// {machine}" — so this is one of the screens #485 is about. It used to
    /// carry `{target:?}`, which for a `Profile` is its config UUID and
    /// nothing else.
    #[test]
    fn an_unroutable_workspace_is_not_named_by_its_profile_uuid() {
        let id = uuid::Uuid::new_v4();
        let gone = RemoteTarget::Profile { id };

        let named = unroutable(gone.clone(), Some("lager"));
        let e = named
            .route_header()
            .expect_err("no spec, no route")
            .to_string();
        assert!(
            !e.contains(&id.to_string()),
            "a bare profile UUID reached the UI: {e}"
        );
        assert!(
            e.contains("lager"),
            "the entry's own name is what it is called: {e}"
        );
        assert!(e.contains("cannot be routed"), "{e}");

        // Nothing to call it by is still no reason to print the UUID.
        let bare = unroutable(gone, None);
        let e = bare
            .route_header()
            .expect_err("no spec, no route")
            .to_string();
        assert!(
            !e.contains(&id.to_string()),
            "a bare profile UUID reached the UI: {e}"
        );
        assert!(e.contains("cannot be routed"), "{e}");
    }
}

/// Issue #430, and #774 for the half of it that was still open.
///
/// Whether the reader puts back a parked cursor is a property of the pty this
/// pane is attached to, not of the platform the client was compiled for — so
/// these drive both answers on every platform, over a real socket pair. Before
/// #774 the decision was `cfg!(windows)`, which meant a Windows client talking
/// to a Linux host repaired a cursor no conhost had parked, and the `wq` typed
/// after vim's `:` landed on the row being edited.
#[cfg(test)]
mod parked_cursor_tests {
    use super::replay_tests::socket_pair;
    use super::*;
    use std::io::Write as _;

    fn terminal_on(pty: PtySource, size: TermSize) -> (RemoteTerminal, Stream) {
        crate::core::config::pin_test_config_dir();
        let (client_side, daemon_side) = socket_pair();
        let term = RemoteTerminal::from_stream_with(client_side, size, Vec::new(), pty)
            .expect("a terminal over a socket pair");
        assert_eq!(term.is_local_conpty(), pty == PtySource::LocalConpty);
        (term, daemon_side)
    }

    /// Feeds one conhost-shaped repaint and reports the cell the cursor ends on,
    /// waiting for the `X` the frame paints so the reader is known to be done.
    fn cursor_after_conpty_frame(pty: PtySource, frame: &[u8]) -> (i32, usize) {
        let (term, mut daemon_side) = terminal_on(pty, TermSize::new(80, 24));

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
    fn a_local_route_is_a_conpty_only_where_conhost_exists() {
        assert_eq!(
            PtySource::for_route(&PaneRoute::Local),
            if cfg!(windows) {
                PtySource::LocalConpty
            } else {
                PtySource::Raw
            },
        );
    }

    #[test]
    fn a_routed_pane_reads_a_raw_pty_whatever_the_client_was_built_for() {
        let spec: NativeSshSpec = serde_json::from_str(
            r#"{"host":"linux-box","port":22,"user":"dev","auth_mode":"auto"}"#,
        )
        .unwrap();
        // A remote workspace only ever installs a `tty7-server` on Linux or
        // macOS (`install::asset::asset_for_uname`), and a WSL workspace is a
        // Linux server too, so a routed pane's pty is a raw one — the far end
        // is never a conhost, whoever is dialling it.
        for header in [
            crate::daemon::router::RouteHeader::ssh(spec),
            crate::daemon::router::RouteHeader::wsl("Ubuntu-22.04"),
        ] {
            let route = PaneRoute::Remote {
                header: Box::new(header),
                resize_echo: false,
            };
            assert_eq!(PtySource::for_route(&route), PtySource::Raw);
        }
    }

    #[test]
    fn a_conpty_frame_that_shows_the_cursor_over_an_erase_keeps_the_cell_it_hid_on() {
        assert_eq!(
            cursor_after_conpty_frame(
                PtySource::LocalConpty,
                b"\x1b[?25l\x1b[20;2HX\x1b[K\x1b[m\x1b[22;42H\x1b[K\x1b[?25h",
            ),
            (5, 3),
            "conhost parked the cursor on the cell it erased last; the cursor \
             belongs where it was when the repaint hid it"
        );
    }

    #[test]
    fn the_same_frame_off_a_raw_pty_leaves_the_cursor_where_the_frame_left_it() {
        assert_eq!(
            cursor_after_conpty_frame(
                PtySource::Raw,
                b"\x1b[?25l\x1b[20;2HX\x1b[K\x1b[m\x1b[22;42H\x1b[K\x1b[?25h",
            ),
            (21, 41),
            "with no conhost in between the stream is the application's own, \
             and the cell it left the cursor on is the cell it meant"
        );
    }

    #[test]
    fn a_conpty_frame_that_moves_the_cursor_before_showing_it_is_obeyed() {
        assert_eq!(
            cursor_after_conpty_frame(
                PtySource::LocalConpty,
                b"\x1b[?25l\x1b[20;2HX\x1b[K\x1b[m\x1b[22;42H\x1b[K\x1b[9;9H\x1b[?25h"
            ),
            (8, 8),
            "the frame painted the cursor somewhere on purpose"
        );
    }

    /// Vim opens its command line with exactly the shape the parked-cursor
    /// scanner calls parked — hide, move around to paint, end on the `:` it
    /// wrote — and then echoes every following keystroke as a bare byte at
    /// wherever that left the cursor. Putting the cursor back on a raw pty
    /// therefore does not straighten out a stray caret, it drops `wq!` onto the
    /// row vim was editing. Bytes below are a capture of vim 9 on a 20x11 pty.
    #[test]
    fn a_raw_pty_repaint_keeps_the_cursor_the_frame_left_so_the_echo_lands_on_it() {
        let (term, mut daemon_side) = terminal_on(PtySource::Raw, TermSize::new(20, 11));

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

    /// The route cannot tell a native-SSH pane from a local shell: both are
    /// spawned through this machine's daemon. What tells them apart is the
    /// `RemoteContext` the daemon sends — and it has to, because a window
    /// reopening onto an existing native-SSH pane attaches by id and has
    /// nothing else to go on.
    #[test]
    fn a_native_ssh_context_turns_the_repair_off_mid_stream() {
        let (term, mut daemon_side) = terminal_on(PtySource::LocalConpty, TermSize::new(80, 24));

        DaemonMsg::RemoteContext(Some(RemoteContext {
            kind: RemoteKind::NativeSsh,
            argv: Vec::new(),
            target: "dev@linux-box".into(),
        }))
        .encode(&mut daemon_side)
        .unwrap();
        daemon_side.flush().unwrap();
        for _ in 0..600 {
            if term.remote_context().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            term.remote_context().is_some(),
            "the reader never applied the context"
        );
        assert!(!term.is_local_conpty());

        DaemonMsg::Output(b"\x1b[6;4H".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        DaemonMsg::Output(b"\x1b[?25l\x1b[20;2HX\x1b[K\x1b[m\x1b[22;42H\x1b[K\x1b[?25h".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        let mut cursor = (0, 0);
        for _ in 0..600 {
            {
                let t = term.term.lock();
                let painted = t.grid()[alacritty_terminal::index::Line(19)]
                    [alacritty_terminal::index::Column(1)]
                .c;
                if painted == 'X' {
                    let point = t.grid().cursor.point;
                    cursor = (point.line.0, point.column.0);
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            cursor,
            (21, 41),
            "an ssh channel's bytes are the far host's own; nothing parked that cursor"
        );
    }
}

/// Issue #774, from the client's end. A full-screen tool sets its modes once
/// and the replay ring drops them, so the daemon re-sends them from what it
/// folded out of the stream ([`tty7_core::core::term_modes`]). This is the
/// other half of that: the bytes it re-sends have to land the emulator back
/// where the application left it, because those are the modes `wheel_route`
/// reads before it decides the pane has a scrollback to move at all.
#[cfg(test)]
mod replayed_mode_tests {
    use super::replay_tests::socket_pair;
    use super::*;
    use std::io::Write as _;
    use tty7_core::core::term_modes::TerminalModes;

    #[test]
    fn a_replayed_mode_frame_puts_the_client_back_on_the_alternate_screen() {
        crate::core::config::pin_test_config_dir();
        // `btop`'s startup prefix, folded the way the daemon folds it out of
        // the pty — and then re-sent from the fold, the ring having dropped
        // the bytes themselves hours ago.
        let mut modes = TerminalModes::new();
        modes.feed(b"\x1b[?1049h\x1b[?1002h\x1b[?1006h");

        let (client_side, mut daemon_side) = socket_pair();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        DaemonMsg::Snapshot(modes.restore_bytes().expect("a fold with modes in it"))
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();

        for _ in 0..600 {
            if term.term.lock().mode().contains(TermMode::ALT_SCREEN) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        let mode = *term.term.lock().mode();
        assert!(
            mode.contains(TermMode::ALT_SCREEN),
            "the pane belongs on the alternate screen it never left: {mode:?}"
        );
        // Reporting first, SGR encoding with it: `wheel_route` sends the wheel
        // to the application on the first and encodes the report with the
        // second — see `wheel_routes_by_negotiated_mode_with_reporting_first`.
        assert!(
            mode.intersects(TermMode::MOUSE_MODE),
            "mouse reporting is what keeps the wheel off the scrollback: {mode:?}"
        );
        assert!(mode.contains(TermMode::SGR_MOUSE), "{mode:?}");
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
            label: None,
            resize_echo: false,
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

        let mut ws = ssh_workspace();
        ws.resize_echo = true;
        assert!(
            matches!(
                PaneRoute::for_workspace(Some(&ws)),
                PaneRoute::Remote {
                    resize_echo: true,
                    ..
                }
            ),
            "what the host's hello said about the resize echo rides the route"
        );
    }

    #[test]
    fn a_wsl_workspace_routes_by_distro() {
        let ws = PaneWorkspace {
            workspace: crate::core::session::WorkspaceId::new(),
            target: crate::core::session::RemoteTarget::Wsl {
                distro: "Ubuntu-22.04".into(),
            },
            spec: None,
            label: None,
            resize_echo: false,
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
            label: None,
            resize_echo: false,
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
            label: None,
            resize_echo: false,
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

    /// #673: a daemon that attached replays the pane's ring at once, and the
    /// ring always holds a segment, so even a pane that has printed nothing
    /// answers with a `Size`. A connection that stays silent for the whole wait
    /// is therefore one nobody is serving — read as success, it became a pane
    /// the window drew as restored while every keystroke, Ctrl-C included,
    /// went into a socket nothing drained.
    #[test]
    fn an_attach_nobody_answers_is_not_an_attach() {
        let (mut client_side, _daemon_side) = UnixStream::pair().unwrap();
        let wait = std::time::Duration::from_millis(200);
        let err = attach_reply_prefix(&mut client_side, 7, wait)
            .expect_err("silence for the whole wait must not pass for an attached pane");
        assert!(
            format!("{err:#}").contains("did not answer"),
            "the failure has to say the daemon never answered, not that the pane is gone: {err:#}"
        );
        assert!(
            attach_unanswered(&err),
            "silence is its own verdict, told apart from a refusal or a hangup"
        );
        // A refusal is not it: the pane really is gone then, and the caller
        // may say so.
        let (mut client_side, mut daemon_side) = UnixStream::pair().unwrap();
        DaemonMsg::Error("no such pane 7".into())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        let refused = attach_reply_prefix(&mut client_side, 7, wait).expect_err("refused");
        assert!(!attach_unanswered(&refused));
    }

    /// The relink retry loop keys off this split: a refusal means the pane is
    /// gone on its machine and asking again can only repeat the answer, while
    /// silence and a hangup are transport trouble worth another try.
    #[test]
    fn a_refused_attach_is_final_and_every_other_failure_is_not() {
        let wait = std::time::Duration::from_millis(200);

        let (mut client_side, mut daemon_side) = UnixStream::pair().unwrap();
        DaemonMsg::Error("no such pane 7".into())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        let refused = attach_reply_prefix(&mut client_side, 7, wait).expect_err("refused");
        assert!(attach_refused(&refused));
        assert!(
            format!("{refused:#}").contains("no such pane 7"),
            "the daemon's own words survive: {refused:#}"
        );

        let (mut client_side, _held_open) = UnixStream::pair().unwrap();
        let silent = attach_reply_prefix(&mut client_side, 7, wait).expect_err("silent");
        assert!(
            !attach_refused(&silent),
            "silence is not a verdict on the pane"
        );

        let (mut client_side, daemon_side) = UnixStream::pair().unwrap();
        drop(daemon_side);
        let hungup = attach_reply_prefix(&mut client_side, 7, wait).expect_err("hung up");
        assert!(
            !attach_refused(&hungup),
            "a hangup is not a verdict on the pane"
        );
    }

    /// The other side of the rule above: the frame a quiet pane does send is
    /// enough. Nothing else is required of the daemon for the attach to stand.
    #[test]
    fn a_size_frame_alone_is_a_live_attach() {
        let (mut client_side, mut daemon_side) = UnixStream::pair().unwrap();
        DaemonMsg::Size(WinSize {
            cols: 80,
            rows: 24,
            cell_w: 8,
            cell_h: 17,
        })
        .encode(&mut daemon_side)
        .unwrap();
        daemon_side.flush().unwrap();

        let buffered =
            attach_reply_prefix(&mut client_side, 7, std::time::Duration::from_millis(200))
                .expect("the daemon's first frame is the attach ack");
        assert!(
            !buffered.is_empty(),
            "the Size frame was read to classify the reply; it must reach the reader"
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
        let term = RemoteTerminal::from_stream_with(
            client_side,
            TermSize::new(80, 24),
            buffered,
            PtySource::Raw,
        )
        .unwrap();

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

        crate::core::config::pin_test_config_dir();
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

    /// Feeds `output` to a pane configured with `configured`, one daemon frame
    /// per chunk, and reports the cursor shape once all of it has been parsed.
    fn cursor_shape_after(
        configured: ConfigCursorStyle,
        output: &[&[u8]],
    ) -> alacritty_terminal::vte::ansi::CursorShape {
        use alacritty_terminal::index::{Column, Line};

        // The pane reads config.json when it is built; keep that off the
        // user's real one.
        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        let mut user_config = crate::core::config::Config::default();
        user_config.cursor_style = configured;
        term.apply_user_config(&user_config);

        for chunk in output {
            DaemonMsg::Output(chunk.to_vec())
                .encode(&mut daemon_side)
                .unwrap();
        }
        // A sentinel painted after everything else: once it is on the grid,
        // every chunk before it has been parsed, so the shape read below is
        // the final one and not a transient match.
        DaemonMsg::Output(b"\x1b[24;1H#".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        daemon_side.flush().unwrap();
        for _ in 0..400 {
            if term.term.lock().grid()[Line(23)][Column(0)].c == '#' {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let term = term.term.lock();
        assert_eq!(
            term.grid()[Line(23)][Column(0)].c,
            '#',
            "output never parsed"
        );
        term.cursor_style().shape
    }

    /// #837: both spellings of the DECSCUSR reset land on `cursor_style`.
    #[test]
    fn cursor_style_reset_returns_to_a_configured_bar_or_underline() {
        use alacritty_terminal::vte::ansi::CursorShape;

        for reset in [&b"\x1b[0 q"[..], &b"\x1b[ q"[..]] {
            for (configured, want) in [
                (ConfigCursorStyle::Bar, CursorShape::Beam),
                (ConfigCursorStyle::Underline, CursorShape::Underline),
            ] {
                assert_eq!(
                    cursor_shape_after(configured, &[b"\x1b[6 q", b"\x1b[2 q", reset]),
                    want,
                    "{configured:?} after {reset:?}"
                );
            }
        }
    }

    /// #837: what nvim actually sends on exit under `xterm-256color` is `Se`,
    /// `\e[2 q` — a literal steady block. The command's `D` hands the prompt
    /// its configured cursor back, whether the block arrives in the same
    /// frame as the marks or in one of its own.
    #[test]
    fn a_block_an_exiting_command_left_is_undone_at_its_finish_mark() {
        use alacritty_terminal::vte::ansi::CursorShape;

        for (configured, want) in [
            (ConfigCursorStyle::Bar, CursorShape::Beam),
            (ConfigCursorStyle::Underline, CursorShape::Underline),
        ] {
            assert_eq!(
                cursor_shape_after(
                    configured,
                    &[b"\x1b]133;C;nvim x\x07\x1b[2 q\x1b]133;D;0\x07$ "],
                ),
                want,
                "{configured:?}, one frame"
            );
            assert_eq!(
                cursor_shape_after(
                    configured,
                    &[
                        b"\x1b]133;C;nvim x\x07",
                        b"\x1b[6 q",
                        b"\x1b[2 q\x1b[?1049l",
                        b"\x1b]133;D;0\x07\x1b]133;A\x07$ ",
                    ],
                ),
                want,
                "{configured:?}, split frames"
            );
        }
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

    /// A peer that holds the link open and stops reading is what the router
    /// looks like from here when the far end is congested: `copy_bidirectional`
    /// stops draining our half and the send buffer fills. The socket is
    /// blocking and has no write timeout, so `write(2)` parks — and it used to
    /// park on the UI thread, which draws every window. macOS gives a unix
    /// stream 8K, so it took about 1400 keystrokes, or one paste.
    ///
    /// Typing into such a pane must now cost the window nothing at all.
    #[test]
    fn a_link_that_stopped_reading_does_not_park_the_writing_thread() {
        crate::core::config::pin_test_config_dir();
        let (client_side, daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // Far past the 8K that used to be fatal, one keystroke at a time, on
        // this very thread: a park anywhere in here is a frozen window.
        let started = std::time::Instant::now();
        for _ in 0..64 * 1024 {
            term.write(vec![b'x']);
        }
        let spent = started.elapsed();

        drop(daemon_side);
        assert!(
            spent < std::time::Duration::from_secs(1),
            "64K keystrokes into a link nobody is draining took {spent:?} — \
             the caller is a gpui event handler, so this is the UI thread"
        );
    }

    /// The backlog is bounded, and reaching the bound is not a slow link but a
    /// dead one: nothing has taken a byte for four megabytes of typing. Saying
    /// so is what stops the pane quietly swallowing input forever, and it is
    /// said in the same words — and once — as an outright refused write.
    #[test]
    fn a_backlog_nothing_drains_is_reported_as_a_lost_link() {
        crate::core::config::pin_test_config_dir();
        let (client_side, daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // 8K a shot rather than a byte, so this is a few hundred frames and not
        // a few million: the bound is on bytes queued, not on frames.
        //
        // Twice the bound rather than a frame or two past it. The sender does
        // get some of this onto the wire before it parks — as much as the send
        // buffer holds, which is 8K on macOS but a couple hundred K on Linux —
        // and that much is discounted from the backlog. A margin narrower than
        // the widest of those buffers is a test that passes on one platform and
        // not the other.
        let chunk = vec![b'x'; 8 << 10];
        for _ in 0..2 * MAX_BACKLOG / chunk.len() {
            term.write(chunk.clone());
        }

        assert!(
            term.exited_flag.load(Ordering::SeqCst),
            "a link that has taken nothing for {MAX_BACKLOG} bytes is gone, and the pane must say so"
        );
        let mut exits = 0;
        while let Ok(ev) = term.events.try_recv() {
            exits += usize::from(matches!(ev, AlacEvent::Exit));
        }
        assert_eq!(exits, 1, "said once, not once per keystroke");
        drop(daemon_side);
    }

    /// One frame can be larger than the whole backlog bound: a paste is
    /// whatever the clipboard holds. That is not a link nothing is draining,
    /// and a pane must not die of being pasted into — nor may the keystrokes
    /// that follow read as a backlog merely because the paste is still going.
    #[test]
    fn a_paste_larger_than_the_backlog_bound_is_not_a_dead_link() {
        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // A peer that keeps reading: the link is healthy, just carrying a lot.
        let draining = std::thread::spawn(move || {
            let _ = std::io::copy(&mut daemon_side, &mut std::io::sink());
        });

        term.write(vec![b'x'; MAX_BACKLOG + (1 << 20)]);
        for _ in 0..1024 {
            term.write(vec![b'y']);
        }

        assert!(
            !term.exited_flag.load(Ordering::SeqCst),
            "a five megabyte paste is a paste, not a link that has stopped taking input"
        );
        drop(term);
        draining.join().unwrap();
    }

    /// Input the link refuses used to vanish: `write` threw the error away, so a
    /// pane whose daemon had stopped reading kept taking keystrokes into
    /// nothing. The refusal now marks the pane exited by the reader's own signal
    /// — only our sending half is shut here, so the reader is still parked on
    /// an open receiving half and the writing side is the one that finds out.
    /// (Shutting the peer's receiving half instead is not portable: Linux
    /// answers the next write with EPIPE, macOS buffers it.)
    ///
    /// The refusal is met on the sender thread now, so the pane learns of it a
    /// moment after the keystroke rather than during it. That is the trade the
    /// queue buys: the window never waits on the socket to find out.
    #[test]
    fn a_write_the_link_refuses_marks_the_pane_gone_once() {
        crate::core::config::pin_test_config_dir();
        let (client_side, _daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        term.link
            .closer
            .shutdown(std::net::Shutdown::Write)
            .unwrap();

        term.write(b"echo hi\r".to_vec());
        let noticed = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !term.exited_flag.load(Ordering::SeqCst) && std::time::Instant::now() < noticed {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            term.exited_flag.load(Ordering::SeqCst),
            "a refused Input is the link gone, and the pane has to say so"
        );
        let mut exits = 0;
        while let Ok(ev) = term.events.try_recv() {
            exits += usize::from(matches!(ev, AlacEvent::Exit));
        }
        assert_eq!(
            exits, 1,
            "reported through the same event the reader raises on EOF"
        );

        term.write(b"echo again\r".to_vec());
        while let Ok(ev) = term.events.try_recv() {
            exits += usize::from(matches!(ev, AlacEvent::Exit));
        }
        assert_eq!(exits, 1, "said once, not once per keystroke");
    }

    /// A link retired for a relink takes no more input — `stop_reader` closes
    /// the queue and shuts the socket down — and that must not read as the pane
    /// dying under the swap, for the same reason the retired reader exits
    /// silently. The frame is now turned away at the queue rather than by the
    /// socket, but the pane has to stay alive either way.
    #[test]
    fn a_write_on_a_retired_link_does_not_mark_the_pane_gone() {
        crate::core::config::pin_test_config_dir();
        let (client_side, _daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        term.stop_reader();

        term.write(b"echo hi\r".to_vec());
        assert!(
            !term.exited_flag.load(Ordering::SeqCst),
            "the pane is being relinked or released, not dying"
        );
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
        // A remote route whose host advertised the echo. The local daemon's
        // answer rides the same gate through `local_daemon_supports`, so this
        // covers the deferral for both.
        term.route = PaneRoute::Remote {
            header: Box::new(crate::daemon::router::RouteHeader::wsl("Ubuntu-22.04")),
            resize_echo: true,
        };

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

    /// A route whose daemon promised to echo every resize, over a socket pair
    /// the test plays the daemon on. The switch tests build the same route
    /// against a real pane.
    fn echoing_pair() -> (RemoteTerminal, UnixStream) {
        let (client_side, daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        term.route = PaneRoute::Remote {
            header: Box::new(crate::daemon::router::RouteHeader::wsl("Ubuntu-22.04")),
            resize_echo: true,
        };
        daemon_side
            .set_read_timeout(Some(std::time::Duration::from_millis(150)))
            .unwrap();
        (term, daemon_side)
    }

    /// The geometry of the next frame the pane sent, if it sent one at all.
    fn next_resize(daemon_side: &mut UnixStream) -> Option<(u16, u16)> {
        match ClientMsg::read(daemon_side) {
            Ok(ClientMsg::Resize(ws)) => Some((ws.cols, ws.rows)),
            Ok(other) => panic!("expected a Resize, got {other:?}"),
            Err(_) => None,
        }
    }

    /// Makes the last resize old enough that an echo for it is no longer
    /// expected, without the test sitting out the grace for real.
    fn outlive_the_echo_grace(term: &mut RemoteTerminal) {
        term.resize_sent_at = std::time::Instant::now()
            .checked_sub(RESIZE_ECHO_GRACE + std::time::Duration::from_millis(1));
    }

    fn wait_for_columns(term: &RemoteTerminal, cols: usize) {
        use alacritty_terminal::grid::Dimensions as _;
        for _ in 0..400 {
            if term.term.lock().columns() == cols {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the grid never reached {cols} columns");
    }

    /// #893: a pane came back from sleep or a workspace switch painting a grid
    /// shorter than its bounds, and stayed that way until a divider drag. On an
    /// echoing route the grid only moves when the daemon's `Size` comes back,
    /// and the early-out trusted the request: once asked for, the same size
    /// was never sent again, so a request or echo that went missing stranded
    /// the grid — and the child — at the old geometry for good.
    #[test]
    fn an_echoed_resize_whose_echo_never_came_is_asked_for_again() {
        use alacritty_terminal::grid::Dimensions as _;

        let (mut term, mut daemon_side) = echoing_pair();
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(next_resize(&mut daemon_side), Some((120, 30)));

        // Every frame asks again. While the echo can still be on its way that
        // is not a reason to resend.
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(
            next_resize(&mut daemon_side),
            None,
            "an echo still in flight must not be chased with duplicates"
        );

        // The echo never comes. Past the grace the grid is still 80x24, and
        // the frame that notices must ask for the size again.
        outlive_the_echo_grace(&mut term);
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(
            next_resize(&mut daemon_side),
            Some((120, 30)),
            "a resize whose echo never arrived was never retried, so the grid \
             stays at the old size until the layout asks for a different one"
        );
        assert_eq!(
            term.term.lock().columns(),
            80,
            "the retry still waits for the echo rather than reflowing early"
        );

        // Once the echo lands the size is settled, however old the request.
        DaemonMsg::Size(WinSize {
            cols: 120,
            rows: 30,
            cell_w: 8,
            cell_h: 17,
        })
        .encode(&mut daemon_side)
        .unwrap();
        daemon_side.flush().unwrap();
        wait_for_columns(&term, 120);
        assert_eq!(term.term.lock().screen_lines(), 30);
        outlive_the_echo_grace(&mut term);
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(
            next_resize(&mut daemon_side),
            None,
            "a grid that reached the requested size must not be resized again"
        );
    }

    /// The other way the grid and the request can part on an echoing route: a
    /// `Size` frame that lands after the echo moves the grid off the size the
    /// layout asked for. The non-echoing path already re-asserts there
    /// (`layout_resize_reasserts_geometry_after_a_late_size_frame`); the
    /// echoing one skipped it, since the request itself had not changed.
    #[test]
    fn an_echoed_resize_reasserts_geometry_after_a_late_size_frame() {
        let (mut term, mut daemon_side) = echoing_pair();
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(next_resize(&mut daemon_side), Some((120, 30)));
        for (cols, rows) in [(120, 30), (100, 12)] {
            DaemonMsg::Size(WinSize {
                cols,
                rows,
                cell_w: 8,
                cell_h: 17,
            })
            .encode(&mut daemon_side)
            .unwrap();
        }
        daemon_side.flush().unwrap();
        wait_for_columns(&term, 100);

        outlive_the_echo_grace(&mut term);
        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(
            next_resize(&mut daemon_side),
            Some((120, 30)),
            "a grid knocked off the requested size was left there"
        );
    }

    /// A relink re-sends the geometry the pane has now — including a resize
    /// made after the old link had already died, which reached no daemon.
    #[test]
    fn a_relink_delivers_the_resize_made_while_the_link_was_down() {
        crate::core::config::pin_test_config_dir();
        let (old_client, mut old_daemon) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(old_client, TermSize::new(80, 24)).unwrap();
        term.resize(TermSize::new(100, 40), 8, 17);
        assert!(matches!(
            ClientMsg::read(&mut old_daemon).unwrap(),
            ClientMsg::Resize(ws) if ws.cols == 100 && ws.rows == 40
        ));

        // The link drops; the layout keeps moving.
        drop(old_daemon);
        term.resize(TermSize::new(120, 30), 8, 17);

        let (new_client, mut new_daemon) = UnixStream::pair().unwrap();
        new_daemon
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        // What `relink_plan` hands the relink: the size the view has now.
        let size = term.size();
        term.adopt_relink(new_client, Vec::new(), &PaneRoute::Local, size, 8, 17)
            .unwrap();
        assert!(
            matches!(
                ClientMsg::read(&mut new_daemon).unwrap(),
                ClientMsg::Resize(ws) if ws.cols == 120 && ws.rows == 30
            ),
            "the relinked daemon must be told the size the pane has now"
        );
    }
    #[test]
    fn a_remote_route_without_the_advertised_echo_reflows_at_request_time() {
        use alacritty_terminal::grid::Dimensions as _;

        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let mut term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        // What `for_workspace` builds when the host's hello named no echo —
        // an older server, or a link that was down when the route was made.
        term.route = PaneRoute::Remote {
            header: Box::new(crate::daemon::router::RouteHeader::wsl("Ubuntu-22.04")),
            resize_echo: false,
        };

        term.resize(TermSize::new(120, 30), 8, 17);
        assert_eq!(
            term.term.lock().columns(),
            120,
            "with no promised echo the grid must reflow at request time — \
             deferring would leave it at the old width forever"
        );
        assert!(matches!(
            ClientMsg::read(&mut daemon_side).unwrap(),
            ClientMsg::Resize(ws) if ws.cols == 120 && ws.rows == 30
        ));

        // The other route with no daemon to promise anything.
        term.route = PaneRoute::Unroutable("no ssh details".into());
        assert!(!term.resize_echoed(), "no route, no echo to wait for");
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

    /// #654, end to end on the reader: a shell at its prompt (the daemon's
    /// `Prompt` report) whose clock sits one column short of the edge, the
    /// pane narrowed by several columns at a time and widened back, each step
    /// followed by zsh's SIGWINCH redraw. Every live echo has to hand the
    /// prompt back before reflowing, or each narrowing strands the head of the
    /// old prompt and the widening joins them into a line of fragments.
    #[test]
    fn a_live_size_echo_at_a_prompt_leaves_one_prompt() {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line};

        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(98, 12)).unwrap();
        // zsh 5.9's redraw for `PROMPT='[~] '` and an `RPROMPT` clock.
        let redraw = |cols: u16, clock: &str| {
            format!(
                "\r\r\x1b[0m\x1b[J[~] \x1b[K\x1b[{}C{clock}\x1b[{}D",
                cols - 13,
                cols - 5
            )
            .into_bytes()
        };
        DaemonMsg::Prompt {
            active: true,
            at_prompt: true,
            last_exit: Some(0),
        }
        .encode(&mut daemon_side)
        .unwrap();
        DaemonMsg::Output(redraw(98, "16:46:20"))
            .encode(&mut daemon_side)
            .unwrap();
        for cols in [92, 86, 92, 98] {
            DaemonMsg::Size(WinSize {
                cols,
                rows: 12,
                cell_w: 8,
                cell_h: 17,
            })
            .encode(&mut daemon_side)
            .unwrap();
            let clock = if cols == 98 { "16:46:33" } else { "16:46:2x" };
            DaemonMsg::Output(redraw(cols, clock))
                .encode(&mut daemon_side)
                .unwrap();
        }
        daemon_side.flush().unwrap();

        let text = || {
            let t = term.term.lock();
            let grid = t.grid();
            let top = -(grid.history_size() as i32);
            (top..grid.screen_lines() as i32)
                .map(|line| {
                    (0..grid.columns())
                        .map(|col| grid[Line(line)][Column(col)].c)
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .filter(|row| !row.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        };
        for _ in 0..400 {
            if term.term.lock().columns() == 98 && text().contains("16:46:33") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let text = text();
        assert!(
            text.contains("16:46:33"),
            "the last redraw never landed:\n{text}"
        );
        assert_eq!(
            text.matches("[~]").count(),
            1,
            "a narrowing stranded part of an old prompt:\n{text}"
        );
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

    /// The daemon replays a reattached pane's stored agent status as an
    /// ordinary report. Nothing on the wire says so, so the link has to
    /// remember that the first report it hears is that replay — and that
    /// everything after it is live.
    #[test]
    fn a_reattached_link_marks_only_its_first_status_report_as_replayed() {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus};

        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term =
            RemoteTerminal::from_stream_reattached(client_side, TermSize::new(80, 24)).unwrap();
        assert_eq!(
            term.take_replayed_agent_status(),
            None,
            "nothing replayed until the frame actually arrives"
        );

        let report = |status, daemon: &mut UnixStream| {
            DaemonMsg::AgentStatus(Some(AgentSessionState {
                status,
                message: None,
                session_id: Some("sid-1".into()),
                launch_argv: None,
                rich: true,
                cwd: None,
                activity: 0,
                turns: 0,
            }))
            .encode(daemon)
            .unwrap();
            daemon.flush().unwrap();
        };
        let poll = |want: AgentStatus| {
            for _ in 0..200 {
                if term.agent_session().map(|s| s.status) == Some(want) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };

        report(AgentStatus::Done, &mut daemon_side);
        assert!(poll(AgentStatus::Done), "the replayed status should land");
        assert_eq!(
            term.take_replayed_agent_status(),
            Some(Some(AgentStatus::Done)),
            "the first report on a reattached link is stored state"
        );
        assert_eq!(
            term.take_replayed_agent_status(),
            None,
            "only one taker gets it"
        );

        report(AgentStatus::Working, &mut daemon_side);
        assert!(poll(AgentStatus::Working), "the live status should land");
        assert_eq!(
            term.take_replayed_agent_status(),
            None,
            "everything after the replay is something the client watched happen"
        );
    }

    /// A pane with no agent session to replay sends no status frame at all, so
    /// the replay window has to close on its own — otherwise the first report
    /// from an agent launched *later* would be discounted as stored state.
    #[test]
    fn live_output_closes_the_replay_window() {
        use crate::core::cli_agent::{AgentSessionState, AgentStatus};

        crate::core::config::pin_test_config_dir();
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term =
            RemoteTerminal::from_stream_reattached(client_side, TermSize::new(80, 24)).unwrap();

        DaemonMsg::Output(b"$ claude\r\n".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        DaemonMsg::AgentStatus(Some(AgentSessionState {
            status: AgentStatus::Done,
            message: None,
            session_id: Some("sid-1".into()),
            launch_argv: None,
            rich: true,
            cwd: None,
            activity: 0,
            turns: 0,
        }))
        .encode(&mut daemon_side)
        .unwrap();
        daemon_side.flush().unwrap();

        for _ in 0..200 {
            if term.agent_session().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            term.agent_session().map(|s| s.status),
            Some(AgentStatus::Done),
            "the status still lands"
        );
        assert_eq!(
            term.take_replayed_agent_status(),
            None,
            "a report that follows live output is live"
        );
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
            turns: 0,
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

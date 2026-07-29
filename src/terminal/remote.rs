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
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{self, CursorShape, CursorStyle};

use crate::terminal::marks::{MarkEvent, MarkScanner};

use std::collections::VecDeque;

use crate::core::cli_agent::{AgentSessionState, CLIAgent};
use crate::core::config::CursorStyle as ConfigCursorStyle;
use crate::core::osc::OscTokenizer;
use crate::daemon::protocol::{
    AuthPromptKind, AuthResponse, ClientMsg, DaemonMsg, KnownHostEntry, KnownHostId,
    LoopbackForward, LoopbackForwardId, LoopbackForwardInfo, LoopbackForwardRequest,
    ManagedForward, NativeSshSpec, PaneProcs, RemoteContext, SftpEntry, SftpJobProgress, SftpOp,
    SftpOpResult, SftpTransferSpec, ShellSpec, SshForwardRule, SshPhase, WinSize, WorkspaceOp,
    WorkspaceRequest,
};
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
    /// Monotonic count of `Prompt` reports applied. Lets the view tell a
    /// *fresh* prompt (the shell cycled through the submitted command and came
    /// back) from the stale pre-submit state — even when 1 Hz polling misses
    /// the intermediate not-at-prompt window of a fast command.
    seq: u64,
    /// Monotonic count of *entered-prompt edges*: bumped only when a report
    /// flips `at_prompt` false → true. Unlike `seq` it ignores same-prompt
    /// redraws — prompt frameworks re-emit the PS1-embedded `133;B` on every
    /// `reset-prompt` / completion-list reprint, and each re-emission is
    /// another `Prompt` frame. The Tab handoff keys its release off this
    /// (see `TerminalView::editor_handoff`): only a command actually running
    /// (`133;C` → not-at-prompt) starts a new cycle.
    cycle: u64,
}

/// The shared handles the reader thread writes into as daemon frames arrive;
/// `RemoteTerminal` keeps the other ends for the view to read. Bundled so
/// `spawn_reader`'s signature stays readable as signals accrue.
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
    /// FIFO of pending native-SSH auth/host-key prompts (and banners, id 0)
    /// pushed by the reader as `DaemonMsg::AuthPrompt` frames arrive. The view
    /// drains these into the in-pane auth sheet (`ui::ssh_prompt`). Keyed per
    /// pane implicitly — one `RemoteTerminal` is one pane — so switching tabs
    /// never misroutes a prompt.
    auth: Arc<Mutex<VecDeque<(u64, AuthPromptKind)>>>,
    /// Latest native-SSH spawn phase from `DaemonMsg::SshStatus`, for the status
    /// line. `None` until the first status frame (a plain shell pane never sets it).
    phase: Arc<Mutex<Option<SshPhase>>>,
    /// Command marks (OSC 133 prompt positions) for the details panel's Outline.
    marks: crate::terminal::marks::Marks,
}

/// The remote workspace a pane belongs to, and how the local daemon reaches its
/// machine.
///
/// A pane of a remote workspace runs on the *remote* `tty7-server`, so nothing
/// about it is addressable here by `pane_id`. This is what a pane carries
/// instead, and it is the input to every workspace-scoped request: the id says
/// what a forward is *owned* by, the spec says which connection it runs *on*.
/// The two are separate because several workspaces on one machine share one
/// connection but must not share forwards.
///
/// `None` on a `TerminalView` means "not a remote-workspace pane" — a local pane
/// or an SSH pane — and every path here falls back to the pane-addressed one.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneWorkspace {
    /// Identity of the workspace on its machine.
    pub workspace: crate::core::session::WorkspaceId,
    /// How the machine is reached. Read for the WSL special case, which shares
    /// `localhost` with the Windows host and so needs no forward at all.
    pub target: crate::core::session::RemoteTarget,
    /// Names the connection for the daemon's lookup. **Secret-free**
    /// ([`NativeSshSpec::without_secrets`]) — the daemon only matches it against
    /// an already-authenticated connection, so no credential needs to ride here.
    ///
    /// `None` for WSL, which has no SSH connection and needs none.
    pub spec: Option<Box<NativeSshSpec>>,
}

impl PaneWorkspace {
    /// Whether this workspace shares `localhost` with the client, so a
    /// `localhost:PORT` link resolves without any forward (the WSL
    /// exception).
    pub fn shares_localhost(&self) -> bool {
        matches!(self.target, crate::core::session::RemoteTarget::Wsl { .. })
    }

    /// The route header a pane of this workspace opens its connection with.
    ///
    /// **`channel: Pane`, not the default `Control`.** A remote `tty7-server`
    /// listens twice, and the two dialects are not interchangeable: a pane sent
    /// to the control socket gets an `InvalidData` on its first `Spawn`, which
    /// is how "the window opens but nothing runs in it" looked before this
    /// existed.
    ///
    /// The spec travels secret-free, which is deliberate and is what
    /// [`PaneWorkspace::spec`] documents: the daemon matches it against the
    /// connection it already authenticated for this machine's control stream.
    /// If that connection is gone the daemon re-authenticates, and the router's
    /// setup relay is what carries the prompt back here.
    pub fn route_header(&self) -> anyhow::Result<crate::daemon::router::RouteHeader> {
        use crate::core::session::RemoteTarget;
        use crate::daemon::router::RouteHeader;
        let header = match (&self.target, &self.spec) {
            (RemoteTarget::Wsl { distro }, _) => RouteHeader::wsl(distro.clone()),
            // Like WSL, this target carries its own address and needs no spec.
            //
            // `--pane` is added *here* rather than by the router: `LocalStdio`
            // runs the argv verbatim (there is no shell command line for
            // `RouteChannel::bridge_command` to rewrite), so the caller is the
            // only one that can pick the dialect. Same choice the SSH path makes
            // one layer down, made explicit.
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

/// Where a pane's daemon connection lands.
///
/// A pane is the *only* thing in tty7 that can be on a different machine from
/// the window showing it, and this is the whole of how it says so. The transport
/// underneath is identical either way — the same local socket, the same
/// `try_clone`, the same reader thread — because the local daemon forwards a
/// routed connection byte for byte.
#[derive(Clone, Debug, Default)]
pub enum PaneRoute {
    /// This machine's daemon. Every pane before remote workspaces existed, and
    /// still every pane of a local window: **not one byte on the wire changes**
    /// for these, because [`connect_routed`] writes nothing extra.
    #[default]
    Local,
    /// A remote workspace's machine. The connection opens with a route header
    /// and does not carry a `ClientMsg` until the daemon has acked it.
    Remote(Box<crate::daemon::router::RouteHeader>),
    /// A pane that belongs to a remote workspace whose machine cannot be
    /// addressed — no SSH details on file for it.
    ///
    /// **Not `Local`.** Falling back to the local daemon would send this pane's
    /// `Kill { pane_id }` to a daemon where that id names somebody else's pane,
    /// so a route that cannot be built has to fail rather than land somewhere.
    /// Every connection through this variant returns the reason.
    Unroutable(String),
}

impl PaneRoute {
    /// The route a pane of `workspace` takes; [`PaneRoute::Local`] when the pane
    /// belongs to no remote workspace.
    ///
    /// Infallible on purpose: the callers that need a route most are the ones
    /// with nowhere to put an error (a close, a restore probe), and for those
    /// [`PaneRoute::Unroutable`] is the safe answer rather than the local
    /// daemon. The reason still surfaces — at connect time, from the one place
    /// that has somewhere to report it.
    pub fn for_workspace(workspace: Option<&PaneWorkspace>) -> PaneRoute {
        match workspace {
            None => PaneRoute::Local,
            Some(ws) => match ws.route_header() {
                Ok(header) => PaneRoute::Remote(Box::new(header)),
                Err(e) => PaneRoute::Unroutable(e.to_string()),
            },
        }
    }

    /// The header this route prefixes its connection with, or `None` when it
    /// prefixes nothing.
    ///
    /// The single place that decides whether a connection carries an extra
    /// frame, so "a local pane's wire bytes are unchanged" is one assertion
    /// rather than a reading of [`connect_routed`].
    pub fn header(&self) -> Option<&crate::daemon::router::RouteHeader> {
        match self {
            PaneRoute::Remote(header) => Some(header),
            PaneRoute::Local | PaneRoute::Unroutable(_) => None,
        }
    }

    /// Whether this pane's failures are the *local* daemon's to answer for.
    ///
    /// The distinction is not cosmetic. On a routed pane the local daemon is a
    /// byte forwarder: a connection that drops mid-`Spawn` says the far end
    /// failed, and the local daemon is fine. Recovery paths that restart it —
    /// which drains and kills every pane it hosts — would then let one
    /// unreachable remote destroy all of the user's local sessions.
    ///
    /// `Unroutable` counts as not-local for the same reason: nothing was ever
    /// asked of the local daemon, so nothing about it is worth restarting.
    pub fn is_local(&self) -> bool {
        matches!(self, PaneRoute::Local)
    }
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
    /// Trusted foreground remote context, last reported by the daemon.
    remote_context: Arc<Mutex<Option<RemoteContext>>>,
    /// Set true by the reader thread once the child exits or the daemon
    /// disconnects. `poll_exited()` copies this into the `exited` field.
    exited_flag: Arc<AtomicBool>,
    /// Set true only on a *genuine* child exit (`DaemonMsg::Exited` — the
    /// shell ended: `exit`, Ctrl-D, a crash), never on a daemon disconnect or
    /// protocol desync, which also flip `exited_flag`. The distinction gates
    /// pane auto-close: a pane whose shell ended closes itself, while a pane
    /// that merely lost its connection stays visible (auto-closing it would
    /// silently discard — and `close_tab` would try to kill — a session that
    /// may still be alive daemon-side).
    child_exited: Arc<AtomicBool>,
    /// Whether zle is reading the keyboard right now, sniffed client-side from
    /// *live* OSC 133 marks: `B` (prompt end — zle takes over immediately
    /// after) arms it, any other mark disarms it, and Snapshot replays never
    /// touch it (a historical `B` says nothing about now). Gates the typeahead
    /// wipe: a `^U` written before zle reads is kernel-echoed as literal junk.
    zle_reading: Arc<AtomicBool>,
    /// Whether the shell reports vi editing mode for the current prompt. Sniffed
    /// client-side from tty7's shell integration marker (`OSC 133;V;0/1`) so the
    /// daemon/client wire protocol stays compatible across versions.
    shell_vi_mode: Arc<AtomicBool>,
    /// Pending native-SSH auth/host-key prompts, filled by the reader thread. The
    /// view drains these each event batch (`take_auth_prompt`) into the in-pane
    /// sheet. Shared with the reader thread.
    auth_prompts: Arc<Mutex<VecDeque<(u64, AuthPromptKind)>>>,
    /// Latest native-SSH spawn phase (`SshStatus`), for the status line.
    ssh_phase: Arc<Mutex<Option<SshPhase>>>,
    /// The endpoint (`host`, `port`) this pane connected to, retained from the
    /// `NativeSshSpec` at spawn so the auth sheet can build the keychain account
    /// (`user@host:port`) for a "remember" checkbox — the `Password` prompt only
    /// carries user+host, not the port. `None` for non-native panes.
    ssh_endpoint: Option<(String, u16)>,
    /// Whether this connect attempt was launched with a keychain-resolved stored
    /// password pre-filled into the spec. Drives FR-A6: a `Password` prompt that
    /// arrives *after* an auto-supplied stored password means the server rejected
    /// it, so the sheet warns and offers to overwrite/clear the stale entry.
    auto_supplied_password: bool,
    /// The third-party CLI coding agent running in the pane's foreground, last
    /// reported by the daemon via `Agent` (detected from the foreground `argv`).
    /// `None` when no known agent runs. Drives the tab avatar's brand mark — see
    /// [`crate::core::cli_agent`].
    agent: Arc<Mutex<Option<CLIAgent>>>,
    /// The agent's rich session status (idle/working/waiting/done + native
    /// session id), last reported by the daemon via `AgentStatus`. Drives the
    /// status dot, "needs your input" notifications, and session resume.
    agent_session: Arc<Mutex<Option<AgentSessionState>>>,
    /// Command marks recorded by the reader thread from OSC 133, for the details
    /// panel's Outline. Positions are grid rows, so they can only be taken here
    /// on the client — the daemon has no grid.
    marks: crate::terminal::marks::Marks,
    /// Which machine this pane's connection landed on. Kept so the *other*
    /// operations a pane needs — `Kill`, a `List` at restore — go to the same
    /// daemon the pane lives in. A remote pane's id means nothing here, and
    /// sending `Kill { pane_id }` to the local daemon would name whichever local
    /// pane happened to be allocated the same number.
    route: PaneRoute,
    /// The event sink the reader thread publishes through, kept so a
    /// [`relink`](Self::relink) can start a *new* reader against the *same*
    /// channel. The view subscribes to `events` once, at construction, and
    /// never again — a relink that handed the daemon a fresh channel would
    /// leave the pane on screen and permanently deaf.
    proxy: EventProxy,
    reader_thread: Option<JoinHandle<()>>,
}

impl RemoteTerminal {
    /// Connect to the daemon, spawn a fresh pane (shell) sized to `size`, and
    /// start mirroring it. `shell` is the user's dropdown pick, overriding the
    /// daemon's default shell resolution; `None` spawns the default. Returns
    /// the terminal plus the daemon-assigned `pane_id` (the caller persists it
    /// for later session restore / `attach`).
    pub fn spawn(
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cwd: Option<PathBuf>,
        shell: Option<ShellSpec>,
    ) -> anyhow::Result<(Self, u64)> {
        Self::spawn_on(&PaneRoute::Local, size, cell_w, cell_h, cwd, shell, None)
    }

    /// [`spawn`](Self::spawn) onto a particular machine.
    ///
    /// The retry ladder below is about the **local** daemon — the one this
    /// process starts and owns — so it applies unchanged to a routed pane: a
    /// route header cannot be written to a socket nobody is listening on either.
    /// What it deliberately does *not* do is restart anything on the far side; a
    /// remote daemon that is missing or mismatched is `install`'s business, and
    /// it has already run by the time the ack arrives.
    /// `owner` is the workspace the pane will belong to. It only ever reaches
    /// the wire for a **local** spawn against a daemon that advertises
    /// `pane-owner` — the gate lives in [`spawn_once`](Self::spawn_once), so
    /// the retry legs (which may talk to a *different*, freshly started
    /// daemon) re-decide it per attempt.
    pub fn spawn_on(
        route: &PaneRoute,
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cwd: Option<PathBuf>,
        shell: Option<ShellSpec>,
        owner: Option<String>,
    ) -> anyhow::Result<(Self, u64)> {
        let retry_cwd = cwd.clone();
        let retry_shell = shell.clone();
        let retry_owner = owner.clone();
        match Self::spawn_once(route, size, cell_w, cell_h, cwd, shell, owner) {
            Ok(term) => Ok(term),
            Err(first_err) if daemon_not_listening(&first_err) => {
                // Nothing is on the socket: the daemon died (crash, OOM, a stray
                // `kill`) since the last pane was opened. Every later spawn would
                // fail the same way, so bring one back up and retry rather than
                // leaving the window unable to open another terminal.
                if let Err(start_err) = crate::daemon::spawn::ensure_running() {
                    return Err(anyhow::anyhow!(
                        "daemon not running ({first_err}); starting one failed: {start_err}"
                    ));
                }
                Self::spawn_once(route, size, cell_w, cell_h, retry_cwd, retry_shell, retry_owner)
                    .map_err(|second_err| {
                        anyhow::anyhow!(
                            "daemon not running ({first_err}); started one but Spawn still failed: {second_err}"
                        )
                    })
            }
            // **Local panes only.** On a routed pane the connection this reads
            // as "disconnected" belongs to the *remote* — the local daemon is
            // only forwarding bytes across it, and it is fine. Restarting it
            // would not fix anything on the far side, and `restart` drains and
            // kills every pane it hosts: one unreachable remote would take out
            // all of the user's local sessions. Report the far end's failure
            // instead.
            Err(first_err)
                if route.is_local() && daemon_disconnected_before_spawn_reply(&first_err) =>
            {
                // A live-but-old daemon can accept the connection, panic while
                // handling Spawn, and close before replying. Restart once so an
                // upgraded GUI cuts over cleanly instead of crashing on a stale
                // background service.
                if let Err(restart_err) = crate::daemon::spawn::restart() {
                    return Err(anyhow::anyhow!(
                        "daemon disconnected before Spawn reply ({first_err}); restart failed: {restart_err}"
                    ));
                }
                Self::spawn_once(route, size, cell_w, cell_h, retry_cwd, retry_shell, retry_owner).map_err(|second_err| {
                    anyhow::anyhow!(
                        "daemon disconnected before Spawn reply ({first_err}); restarted daemon but Spawn still failed: {second_err}"
                    )
                })
            }
            Err(err) => Err(err),
        }
    }

    fn spawn_once(
        route: &PaneRoute,
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cwd: Option<PathBuf>,
        shell: Option<ShellSpec>,
        owner: Option<String>,
    ) -> anyhow::Result<(Self, u64)> {
        let mut stream = connect_routed(route)?;
        let win = win_size(size, cell_w, cell_h);

        // An owner only goes on the wire when this daemon is known to read the
        // `SPAWN_OWNED` frame — an older one drops the connection over the
        // unknown kind. Local only for now: a routed spawn's capability set is
        // the *remote* server's, which nothing here has interrogated.
        let owner = owner.filter(|_| {
            route.is_local()
                && crate::daemon::spawn::local_daemon_supports(
                    crate::daemon::protocol::FEATURE_PANE_OWNER,
                )
        });

        // Ask the daemon to create the pane, then read its assigned id back. The
        // very next frames on this connection are this pane's Snapshot + Output,
        // which the reader thread (started below) will consume.
        ClientMsg::Spawn {
            cwd,
            size: win,
            shell,
            owner,
        }
        .encode(&mut stream)?;
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

        let mut term = Self::from_stream(stream, size)?;
        term.route = route.clone();
        Ok((term, pane_id))
    }

    /// Connect to the daemon and re-attach to an existing pane `pane_id`, then
    /// start mirroring it. The daemon answers with a `Snapshot` (its byte ring)
    /// that the reader thread replays to rebuild the current screen + scrollback,
    /// followed by live `Output`.
    pub fn attach(size: TermSize, cell_w: u16, cell_h: u16, pane_id: u64) -> anyhow::Result<Self> {
        Self::attach_on(&PaneRoute::Local, size, cell_w, cell_h, pane_id)
    }

    /// [`attach`](Self::attach) on a particular machine. A remote workspace's
    /// pane ids are the *remote* daemon's, so a reattach has to take the same
    /// route the spawn did or it would find a stranger's pane — or, far more
    /// likely, none.
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
        // Far enough into the reply to know whether the pane is still there.
        // Everything read here is handed to the reader thread rather than
        // consumed: a successful attach's first frame is part of the replay.
        let buffered = attach_reply_prefix(&mut stream, pane_id, attach_reply_wait(route))?;
        let mut term = Self::from_stream_with(stream, size, buffered)?;
        term.route = route.clone();
        Ok(term)
    }

    // ── The pane half of a reconnect ────────────────────────────────
    //
    // For one pane: **reopen the channel, `Attach`, take the replay, resize to
    // this client's geometry.** It happens *in place* — the same `Term`, the
    // same event channel, the same shared signals the view already holds
    // handles to. Building a fresh `RemoteTerminal` and swapping it into the
    // view would look simpler and would silently break the pane: the view's
    // event pump subscribes to `events` once, at construction, and would go on
    // listening to the dead terminal's channel for ever.

    /// The **blocking** half of a relink: reach the machine and re-`Attach`.
    ///
    /// Split from [`adopt_relink`](Self::adopt_relink) because this is a
    /// network round trip — an SSH connect on a cold machine, possibly with a
    /// password sheet in the middle — and the terminal it is for is a gpui
    /// entity that can only be touched on the UI thread. So the wait happens on
    /// a background task and only the cheap swap runs where the view lives.
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

    /// The **cheap** half: adopt an already-attached stream from
    /// [`open_relink`](Self::open_relink) as this pane's link.
    ///
    /// # Why the grid is reset first
    ///
    /// The daemon answers `Attach` by replaying its `ReplayRing` from the
    /// start. Advancing that onto a grid that still holds the pre-disconnect
    /// screen would append a second copy of everything. So the mirror is reset
    /// and the machine's own record becomes the whole truth — which is also the
    /// honest presentation of the replay boundary: the ring holds
    /// 8 MiB, a pane that outran it comes back with the daemon's current grid
    /// and **the middle is genuinely gone**. Nothing here interpolates it, and
    /// nothing upstream may imply it will fill in later.
    pub fn adopt_relink(
        &mut self,
        stream: Stream,
        route: &PaneRoute,
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
    ) -> anyhow::Result<()> {
        // Retire the old link first. No `Detach`: this path exists because the
        // socket is already gone, and on the one case where it is not (a
        // deliberate re-attach) the server treats a closed stream as a detach
        // anyway.
        if let Ok(writer) = self.writer.lock() {
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        // The retired reader has been joined, so everything it will ever emit is
        // already in the channel — including its `Exit`. Left there it would be
        // delivered *after* the swap and put "process exited" on a pane that is
        // demonstrably alive. Dropping the rest of that backlog is right for the
        // same reason the grid is reset below: it describes a screen the replay
        // is about to redraw from the machine's own record.
        while self.events.try_recv().is_ok() {}

        let read_half = stream.try_clone()?;

        // The dead link set these on its way out (`teardown`). A pane that is
        // being re-attached is by definition not finished, so they go back —
        // except `child_exited`, which records that the *shell* ended and is
        // still true no matter how many times the client reconnects.
        self.exited_flag.store(false, Ordering::SeqCst);
        self.exited = false;
        {
            use alacritty_terminal::vte::ansi::Handler as _;
            let mut term = self.term.lock();
            term.reset_state();
        }

        let reader = Self::spawn_reader(
            self.term.clone(),
            self.proxy.clone(),
            read_half,
            // Nothing pre-read: unlike `attach_on`, a relink does not classify
            // the reply. A pane that is gone leaves this one disconnected on
            // purpose — the supervisor's retry is the answer here, and spawning
            // a fresh shell into a pane the user is still looking at would
            // discard the screen it is showing.
            Vec::new(),
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
                auth: self.auth_prompts.clone(),
                phase: self.ssh_phase.clone(),
                marks: self.marks.clone(),
            },
        );
        if let Ok(mut writer) = self.writer.lock() {
            *writer = stream;
        }
        self.reader_thread = Some(reader);
        self.route = route.clone();
        // The last step: "以新客户端的尺寸 Resize". `Attach` carries a
        // size but deliberately does not resize the PTY, so the geometry only
        // becomes real when this frame lands — and `synced_size = false` is what
        // lets it through when the size happens to equal the last one.
        self.synced_size = false;
        self.resize(size, cell_w, cell_h);
        Ok(())
    }

    /// Shared tail of `spawn`/`attach`: build the local `Term`, split the socket
    /// into read/write halves, and launch the reader thread.
    pub(super) fn from_stream(stream: Stream, size: TermSize) -> anyhow::Result<Self> {
        Self::from_stream_with(stream, size, Vec::new())
    }

    /// [`from_stream`](Self::from_stream) for a caller that has already read
    /// part of the stream. `buffered` is where the reader thread starts, ahead
    /// of anything still on the socket — `attach_reply_prefix` reads far enough
    /// to classify the reply, and those bytes are the front of the replay.
    pub(super) fn from_stream_with(
        stream: Stream,
        size: TermSize,
        buffered: Vec<u8>,
    ) -> anyhow::Result<Self> {
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
        let auth_prompts: Arc<Mutex<VecDeque<(u64, AuthPromptKind)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let ssh_phase: Arc<Mutex<Option<SshPhase>>> = Arc::new(Mutex::new(None));
        let marks = crate::terminal::marks::Marks::new();

        let reader_thread = Self::spawn_reader(
            term.clone(),
            proxy.clone(),
            read_half,
            buffered,
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
                auth: auth_prompts.clone(),
                phase: ssh_phase.clone(),
                marks: marks.clone(),
            },
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
            remote_context,
            exited_flag,
            child_exited,
            zle_reading,
            shell_vi_mode,
            auth_prompts,
            ssh_phase,
            ssh_endpoint: None,
            auto_supplied_password: false,
            agent,
            agent_session,
            marks,
            // Overwritten by the routed constructors; `from_stream` itself is
            // handed a stream whose destination it cannot see.
            route: PaneRoute::Local,
            proxy,
            reader_thread: Some(reader_thread),
        })
    }

    /// Close this pane's link, leaving the pane running on its machine.
    ///
    /// The same two frames `Drop` sends, without dropping: the
    /// takeover needs the client to *stop being attached* while the view stays
    /// on screen in its read-only state.
    pub fn detach_link(&mut self) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = ClientMsg::Detach.encode(&mut *writer);
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
        // The reader observes the close and runs its own teardown, so the pane
        // lands in exactly the state a dropped network link leaves it in — which
        // is the state wanted after a takeover, reached by the code
        // path that is already exercised every time a connection fails.
        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        self.poll_exited();
    }

    pub fn apply_user_config(&self, user_config: &crate::core::config::Config) {
        let mut term = self.term.lock();
        term.set_options(terminal_config_from_user(user_config));
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
        // Bytes already off the socket (see `from_stream_with`), which the loop
        // resumes from before its first read.
        buffered: Vec<u8>,
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
                    auth,
                    phase,
                    marks,
                } = signals;
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
                // Sniffs tty7's OSC 133;V edit-mode metadata from both replayed
                // snapshots and live output. Unlike zle_reading, this is durable
                // prompt state: an attached client should inherit the last mode
                // marker already present in the replay ring.
                let mut mode_tok = OscTokenizer::new(&[b"133"]);
                // Sniffs OSC 133 marks out of the live stream to track whether
                // zle is reading (see the `zle_reading` field docs). Historical
                // Snapshot replays deliberately do not feed this tokenizer.
                let mut zle_tok = OscTokenizer::new(&[b"133"]);
                // Positional OSC 133 marks for the details panel's Outline. Unlike
                // the tokenizers above this one reports byte *offsets*, because a
                // mark's value is the grid row it lands on — see `terminal::marks`.
                let mut mark_scan = MarkScanner::new();
                // Bytes read but not yet framed, plus the recorded geometry
                // waiting for its paired Snapshot: the attach replay is a
                // `Size` → `Snapshot` pair per ring segment, and each pair
                // must apply under ONE grid lock — with two separate lock
                // scopes, the UI thread's layout `resize()` could slot in
                // between and that segment would replay at the layout width,
                // mis-wrapping history (the exact defect the Size frame
                // exists to prevent). The guarantee is per pair: a layout
                // resize landing *between* pairs only re-reflows already-
                // applied history, and the next pair's Size (ultimately the
                // final pair, which carries the PTY's current geometry)
                // restores the recorded width before more bytes advance.
                let mut pending: Vec<u8> = buffered;
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
                                // Where the batch's OSC 133 marks land, so the
                                // advance can stop at each one and read the grid
                                // row it fell on. Scanned before the lock (it's a
                                // pure byte pass) and normally empty — a batch
                                // with no marks takes the single-advance path
                                // below, exactly as before.
                                let mut cuts: Vec<(usize, MarkEvent)> = Vec::new();
                                mark_scan.feed(&out_batch, |off, ev| cuts.push((off, ev)));
                                {
                                    let t0 = trace.then(std::time::Instant::now);
                                    let mut term = term.lock();
                                    let t1 = trace.then(std::time::Instant::now);
                                    if cuts.is_empty() {
                                        processor.advance(&mut *term, &out_batch);
                                    } else {
                                        let mut at = 0usize;
                                        for (off, ev) in cuts {
                                            processor.advance(&mut *term, &out_batch[at..off]);
                                            at = off;
                                            record_mark(&term, &marks, ev);
                                        }
                                        processor.advance(&mut *term, &out_batch[at..]);
                                    }
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
                                mode_tok.feed(&out_batch, |payload| {
                                    if let Some(mode) = payload.strip_prefix(b"133;V;") {
                                        shell_vi_mode.store(
                                            mode.first() == Some(&b'1'),
                                            Ordering::Relaxed,
                                        );
                                    }
                                });
                                // Live 133 marks: `B` = prompt fully printed, zle
                                // takes the keyboard right after; anything else
                                // (C command start, D precmd, A prompt start)
                                // means it isn't reading.
                                zle_tok.feed(&out_batch, |payload| {
                                    if let Some(mark) = payload.strip_prefix(b"133;") {
                                        match mark.first() {
                                            Some(b'B') => {
                                                zle_reading.store(true, Ordering::Relaxed)
                                            }
                                            Some(b'V') => {
                                                shell_vi_mode.store(
                                                    mark.strip_prefix(b"V;")
                                                        .is_some_and(|v| v.first() == Some(&b'1')),
                                                    Ordering::Relaxed,
                                                );
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
                                        seq: guard.seq + 1,
                                        cycle: guard.cycle
                                            + u64::from(at_prompt && !guard.at_prompt),
                                    };
                                }
                                // The shell just reported a fresh prompt, so at
                                // this position in the byte stream no full-screen
                                // program owns the pane. Any TUI state still in
                                // the grid — a stranded alt screen, a DECTCEM-
                                // hidden cursor, mouse/focus reporting, kitty
                                // keyboard flags — is residue from a program that
                                // died without restoring it (an ssh session
                                // dropping mid-TUI is the canonical case: the
                                // restore sequences can never arrive). Feed the
                                // resets through the same parser path as PTY
                                // output, right here between frames: every byte
                                // the dead program did send has already applied
                                // (`flush_batch!` above), and the prompt text /
                                // next command's bytes only come in later frames,
                                // so this can never fight a live program's own
                                // mode changes. Runs on the attach path too —
                                // the daemon sends `Prompt` after `Snapshot` —
                                // so a stale replay ring self-heals on reattach.
                                if active && at_prompt {
                                    let mut term = term.lock();
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
                                // Crossing the local/remote boundary invalidates
                                // the cwd: it names a directory in the namespace
                                // we just left. Drop it so the pane reports none
                                // until the new shell's OSC 7 lands — otherwise
                                // an `exit` from `ssh` leaves the remote's last
                                // path in place, and a local shell without shell
                                // integration never overwrites it, so the local
                                // `git` probe keeps running against it.
                                if let Ok(mut guard) = cwd.lock() {
                                    *guard = None;
                                }
                                if let Ok(mut guard) = remote.lock() {
                                    *guard = ctx;
                                }
                            }
                            // A native-SSH pane's interactive auth/host-key
                            // request. Queue it and wake the view; the sheet is
                            // rendered and its reply sent via `respond_auth`.
                            // Banners (id 0) ride the same queue and the UI shows
                            // them without a reply.
                            DaemonMsg::AuthPrompt { request_id, prompt } => {
                                flush_batch!();
                                if let Ok(mut guard) = auth.lock() {
                                    guard.push_back((request_id, prompt));
                                }
                                proxy.send_event(AlacEvent::Wakeup);
                            }
                            // Native-SSH spawn progress for the status line.
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
                                // Status changes repaint the tab chip / sidebar
                                // dot even when the pane printed nothing.
                                proxy.send_event(AlacEvent::Wakeup);
                            }
                            DaemonMsg::Exited { .. } => {
                                // Child gone: apply what it printed last, then
                                // mark the emulator exited and flip the shared
                                // flag so the next `poll_exited()` surfaces it.
                                // This is the one exit path where the child
                                // *really* ended (vs the connection dying), so
                                // record that before the teardown's events fire
                                // — the view reads it to decide whether the
                                // pane should close itself.
                                flush_batch!();
                                child_exited.store(true, Ordering::SeqCst);
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

    /// Whether the pane's child process genuinely exited (as opposed to the
    /// daemon connection dropping — see the `child_exited` field docs).
    pub fn child_exited(&self) -> bool {
        self.child_exited.load(Ordering::SeqCst)
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

    pub fn remote_context(&self) -> Option<RemoteContext> {
        self.remote_context.lock().ok().and_then(|g| g.clone())
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

    /// Monotonic count of `Prompt` reports applied so far — see
    /// [`ShellState::seq`]. Comparing values from before and after a submit
    /// tells whether the shell has reported back since.
    pub fn prompt_seq(&self) -> u64 {
        self.shell_state.lock().map(|s| s.seq).unwrap_or(0)
    }

    /// Monotonic count of entered-prompt edges — see [`ShellState::cycle`].
    /// Stable across same-prompt redraws (which bump `seq` but not this);
    /// only leaving the prompt for a command and coming back advances it.
    pub fn prompt_cycle(&self) -> u64 {
        self.shell_state.lock().map(|s| s.cycle).unwrap_or(0)
    }

    /// Exit code of the most recently completed foreground command, as sniffed
    /// from OSC 133;D daemon-side. `None` before any command has finished.
    pub fn last_exit_code(&self) -> Option<i32> {
        self.shell_state.lock().ok().and_then(|s| s.last_exit)
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
    /// The third-party CLI coding agent (Claude Code, Codex, …) running in the
    /// pane's foreground, as last reported by the daemon, or `None`. Cheap cache
    /// read — detection runs daemon-side. See [`crate::core::cli_agent`].
    pub fn foreground_agent(&self) -> Option<CLIAgent> {
        self.agent.lock().ok().and_then(|g| *g)
    }

    /// Command marks recorded from OSC 133, oldest first — the Outline's source.
    /// Cheap clone of a shared handle; the caller snapshots via `Marks::list`.
    pub fn marks(&self) -> crate::terminal::marks::Marks {
        self.marks.clone()
    }

    /// The rich agent-session status (idle/working/waiting/done + native
    /// session id), as last reported by the daemon, or `None` when no agent
    /// session is live. Cheap cache read — sniffing runs daemon-side.
    pub fn agent_session(&self) -> Option<AgentSessionState> {
        self.agent_session.lock().ok().and_then(|g| g.clone())
    }

    pub fn zle_reading(&self) -> bool {
        self.zle_reading.load(Ordering::Relaxed)
    }

    pub fn shell_vi_mode(&self) -> bool {
        self.shell_vi_mode.load(Ordering::Relaxed)
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
        Self::list_panes_on(&PaneRoute::Local)
    }

    /// [`list_panes`](Self::list_panes) on a particular machine. A remote
    /// workspace restores from the *remote* daemon's registry; asking the local
    /// one would report every saved leaf as dead and respawn the lot, silently
    /// abandoning whatever was still running there — the precise failure remote
    /// workspaces exist to prevent.
    pub fn list_panes_on(route: &PaneRoute) -> Vec<crate::daemon::protocol::PaneInfo> {
        Self::try_list_panes_on(route).unwrap_or_default()
    }

    /// [`list_panes_on`](Self::list_panes_on) with the failure kept.
    ///
    /// Swallowing the error into an empty list is right for *restore*, where
    /// "no answer" and "nothing alive" lead to the same action (spawn fresh).
    /// It is wrong for anything that **shows** liveness: on this machine an
    /// unreachable daemon really does mean no pane is running, but a routed
    /// `List` that failed says nothing about the remote's registry — the panes
    /// are very probably still there, we just could not ask. A picker that
    /// renders that as "stopped" tells the user their sessions are gone every
    /// time the link hiccups, so the two cases have to stay distinguishable
    /// this far up (see [`crate::terminal::pane_liveness`]).
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

    /// Tell the daemon to terminate a pane's child and forget it, over a
    /// short-lived control connection. Used when the user explicitly closes a tab
    /// or split pane (as opposed to quitting the app, where panes are *detached*
    /// and kept alive for restore). Best-effort: a missing daemon means there's
    /// nothing to kill anyway.
    pub fn kill_pane(pane_id: u64) {
        Self::kill_pane_on(&PaneRoute::Local, pane_id)
    }

    /// [`kill_pane`](Self::kill_pane) on a particular machine.
    ///
    /// Routing this one is not an optimisation. Pane ids are per-daemon, so
    /// `Kill { pane_id }` sent to the wrong daemon does not fail — it succeeds
    /// against a stranger.
    pub fn kill_pane_on(route: &PaneRoute, pane_id: u64) {
        if let Ok(mut stream) = connect_routed(route) {
            let _ = ClientMsg::Kill { pane_id }.encode(&mut stream);
            // Give the daemon a moment to read the frame before the connection
            // closes; a tiny blocking read of EOF is enough to order it.
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

    // ── Native SSH (WS3): auth/host-key prompt plumbing ──────────────────────

    /// Spawn a native russh-backed pane for `spec`, mirroring [`spawn`] but over
    /// the `SpawnNativeSsh` path. The connection's auth/host-key prompts and
    /// status arrive on this pane's own stream and are surfaced via
    /// [`take_auth_prompt`]/[`ssh_phase`]. Returns the terminal + daemon pane id.
    ///
    /// The single place a secret-bearing spec crosses to the daemon; the caller
    /// (the GUI spec-builder, `ui::ssh_connect`) has already resolved keychain
    /// secrets into `spec`.
    pub fn spawn_native_ssh(
        size: TermSize,
        cell_w: u16,
        cell_h: u16,
        cwd: Option<PathBuf>,
        spec: Box<NativeSshSpec>,
    ) -> anyhow::Result<(Self, u64)> {
        // Mirror `spawn`'s stale-daemon protection: a running daemon from a
        // pre-SSH build drops the connection on the unknown message kind without
        // replying (and never sends an Error frame pre-dispatch), which reads as
        // EOF here. Restart it once and retry so the first SSH connect after an
        // upgrade recovers instead of failing.
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
        // Retain what the auth sheet needs before the spec moves onto the wire:
        // the endpoint (for the keychain account) and whether we pre-filled a
        // stored password (FR-A6).
        let endpoint = (spec.host.clone(), spec.port);
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
        term.auto_supplied_password = auto_supplied_password;
        Ok((term, pane_id))
    }

    /// Pop the next pending native-SSH auth/host-key prompt (or banner, id 0), in
    /// FIFO order. `None` when the queue is empty. The view calls this while
    /// draining its event batch.
    pub fn take_auth_prompt(&self) -> Option<(u64, AuthPromptKind)> {
        self.auth_prompts
            .lock()
            .ok()
            .and_then(|mut q| q.pop_front())
    }

    /// Pop the next pending prompt only when it is a banner; a real
    /// (interactive) prompt stays queued. Used while another pane's sheet is
    /// active — popping a real prompt then would drop it (there is no re-queue),
    /// silently failing that pane's auth after the broker timeout.
    pub fn take_auth_banner(&self) -> Option<String> {
        let mut q = self.auth_prompts.lock().ok()?;
        if matches!(q.front(), Some((_, AuthPromptKind::Banner { .. }))) {
            if let Some((_, AuthPromptKind::Banner { text })) = q.pop_front() {
                return Some(text);
            }
        }
        None
    }

    /// Whether any native-SSH prompt is waiting (cheap check the view uses to
    /// decide whether to emit an `AuthPromptReady` up to the app).
    pub fn has_pending_auth(&self) -> bool {
        self.auth_prompts
            .lock()
            .map(|q| !q.is_empty())
            .unwrap_or(false)
    }

    /// The latest native-SSH spawn phase, if any (`None` for a plain pane).
    pub fn ssh_phase(&self) -> Option<SshPhase> {
        self.ssh_phase.lock().ok().and_then(|g| g.clone())
    }

    /// The `(host, port)` this native-SSH pane connected to, for building the
    /// keychain account in the auth sheet. `None` for a non-native pane.
    pub fn ssh_endpoint(&self) -> Option<(String, u16)> {
        self.ssh_endpoint.clone()
    }

    /// Whether this connect pre-supplied a keychain-stored password (FR-A6): a
    /// later `Password` prompt then means the server rejected the stored value.
    pub fn auto_supplied_password(&self) -> bool {
        self.auto_supplied_password
    }

    /// Reply to a `DaemonMsg::AuthPrompt` with the given `request_id`, sending a
    /// `ClientMsg::AuthResponse` over this pane's own connection (the same socket
    /// the prompt arrived on). Best-effort: a dead socket just fails the auth step
    /// daemon-side, which surfaces as the usual disconnect.
    pub fn respond_auth(&self, request_id: u64, response: AuthResponse) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = ClientMsg::AuthResponse {
                request_id,
                response,
            }
            .encode(&mut *writer);
        }
    }

    /// List the daemon's `known_hosts` entries over a short-lived control
    /// connection (for the "SSH → Known hosts" settings section). Empty on any
    /// error.
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

    /// Delete one `known_hosts` entry, returning the refreshed list.
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

    // --- SFTP (Workstream 5) -------------------------------------------------
    //
    // Each is a synchronous one-shot control request modeled on the loopback
    // helpers above: connect, send one `ClientMsg`, read one `DaemonMsg`. SFTP
    // targets a native-SSH pane; the daemon errors if `pane_id` isn't one.

    /// List a remote directory over the pane's SFTP session.
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

    /// Run a one-shot SFTP filesystem operation.
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

    /// Start a background transfer job; returns its id.
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

    /// Cancel a transfer job; returns the pane's refreshed progress list.
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

    /// Poll the transfer jobs for a pane (drives the tray while it is visible).
    pub fn sftp_transfer_list(pane_id: u64) -> Vec<SftpJobProgress> {
        fn query(pane_id: u64) -> anyhow::Result<Vec<SftpJobProgress>> {
            let mut stream = connect()?;
            ClientMsg::SftpTransferList { pane_id }.encode(&mut stream)?;
            match DaemonMsg::read(&mut stream)? {
                DaemonMsg::SftpTransferProgress(jobs) => Ok(jobs),
                other => Err(anyhow::anyhow!(
                    "unexpected reply to SftpTransferList: {other:?}"
                )),
            }
        }
        query(pane_id).unwrap_or_default()
    }

    /// Establish a managed forward (Local/Remote/Dynamic) on a native-SSH pane over
    /// a short-lived control connection; returns the pane's forwards after the add.
    /// One-shot, modeled on `list_loopback_forwards`.
    pub fn add_forward(pane_id: u64, rule: SshForwardRule) -> Vec<ManagedForward> {
        fn query(pane_id: u64, rule: SshForwardRule) -> anyhow::Result<Vec<ManagedForward>> {
            let mut stream = connect()?;
            ClientMsg::AddForward { pane_id, rule }.encode(&mut stream)?;
            match DaemonMsg::read(&mut stream)? {
                DaemonMsg::ForwardList(list) => Ok(list),
                DaemonMsg::Error(msg) => Err(anyhow::anyhow!(msg)),
                other => Err(anyhow::anyhow!("unexpected reply to AddForward: {other:?}")),
            }
        }
        query(pane_id, rule).unwrap_or_default()
    }

    /// Tear down one managed forward by id; returns the pane's remaining forwards.
    pub fn remove_forward(pane_id: u64, forward_id: u64) -> Vec<ManagedForward> {
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
        query(pane_id, forward_id).unwrap_or_default()
    }

    /// List a native-SSH pane's managed forwards.
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

    // ── Remote workspaces ───────────────────────────────────────

    /// Send one workspace-scoped request and return the daemon's reply.
    ///
    /// How long a workspace-addressed request waits for the daemon.
    ///
    /// Generous, because behind it is an SSH round trip to the workspace's own
    /// machine and possibly a connection being established — but finite, which
    /// is the point.
    const WORKSPACE_OP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    /// The counterpart of the `pane_id`-addressed helpers above for a pane that
    /// lives on a *remote workspace*: there is no pane on the local daemon to
    /// name, so the request carries the workspace and a secret-free spec naming
    /// its machine, and the daemon resolves the connection the workspace already
    /// authenticated (`ssh::workspace::handle`).
    ///
    /// `DaemonMsg::Error` is surfaced as an `Err` so callers can show it — a
    /// disconnected workspace has to be *reported*, not silently treated as an
    /// empty list.
    pub fn on_workspace(req: WorkspaceRequest) -> anyhow::Result<DaemonMsg> {
        let mut stream = connect()?;
        // Bounded, because the daemon's answer is not just its own work: it
        // resolves the workspace's SSH connection and, for the forward ops,
        // waits for the *server* to acknowledge a `cancel_tcpip_forward`. On a
        // box that has gone unreachable — lid closed, VPN dropped, which is
        // exactly when someone reaches for Stop Workspace — that acknowledgement
        // never comes. Without a deadline this read parks forever, and the
        // thread with it.
        //
        // Best effort: a transport that will not take a timeout degrades to the
        // old unbounded read rather than failing the request outright.
        let _ = stream.set_read_timeout(Some(Self::WORKSPACE_OP_TIMEOUT));
        ClientMsg::OnWorkspace(Box::new(req)).encode(&mut stream)?;
        match DaemonMsg::read(&mut stream)? {
            DaemonMsg::Error(msg) => Err(anyhow::anyhow!(msg)),
            reply => Ok(reply),
        }
    }

    /// [`on_workspace`](Self::on_workspace) for the calls whose only sane failure
    /// mode is "show nothing": a list the panel is about to render.
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

    /// Build a [`WorkspaceRequest`] for `op` against `ws`, as seen from `view_pane`.
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

    /// A pane's process tree and listening ports, for the details panel. One-shot
    /// over a short-lived control connection, like the forward queries — this is
    /// polled only while the panel is open, so it never rides the pane's hot
    /// output connection.
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

/// Apply one OSC 133 mark at the emulator's current position.
///
/// Called with the terminal lock held and the parser advanced to exactly the
/// mark's byte, so `cursor.point.line` is the row the mark fell on. That row is
/// converted to an index from the top of the scrollback, which is stable as long
/// as history hasn't saturated — see the `terminal::marks` module docs.
fn record_mark(term: &Term<EventProxy>, marks: &crate::terminal::marks::Marks, event: MarkEvent) {
    use alacritty_terminal::grid::Dimensions as _;
    match event {
        MarkEvent::Prompt => {
            let grid = term.grid();
            let row = grid.history_size() as i64 - grid.display_offset() as i64
                + i64::from(grid.cursor.point.line.0);
            marks.begin(row, String::new());
        }
        MarkEvent::Command(cmd) => marks.set_text(cmd),
        MarkEvent::Done(exit) => marks.finish(exit),
    }
}

/// Whether the failure is "nothing is listening on the socket" — the daemon is
/// gone, as opposed to alive but unhappy. On Unix a dead daemon leaves the
/// socket file behind (`ConnectionRefused`) or removed it on the way out
/// (`NotFound`); on Windows the named pipe simply isn't there (`NotFound`).
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

/// How long to wait for the daemon's first frame after an `Attach` before
/// giving up on *classifying* the reply. Not a deadline on the attach — only on
/// being able to tell "this pane is gone" from "this pane has not said anything
/// yet" — so lapsing costs nothing but the old behaviour. The connection is
/// already open by the time the wait starts (the SSH setup happened inside
/// `connect_routed`), so what is being waited on is one round trip.
///
/// **The two routes are not the same wait.** A remote attach runs on a
/// background thread and answers over an SSH channel, so it can afford to be
/// patient. A local one is on the UI thread — `ui::pending_pane` explains why
/// that path stayed synchronous — where the ceiling is a window freeze, and a
/// local daemon that has not answered in two seconds is not about to.
fn attach_reply_wait(route: &PaneRoute) -> std::time::Duration {
    match route.is_local() {
        true => std::time::Duration::from_secs(2),
        false => std::time::Duration::from_secs(15),
    }
}

/// Read the head of an `Attach` reply, turning "no such pane" into an `Err`, and
/// hand back whatever was read so the reader thread starts from it.
///
/// # Why this exists
///
/// `Attach` has no synchronous reply, so for a long time the client's attach
/// could not fail: it wrote the frame and returned `Ok`, and a pane id that was
/// gone showed up much later as the reader thread hitting EOF — which the view
/// paints as `tty7 — disconnected` and deliberately does *not* close, because
/// on a remote workspace a dropped link and a dead pane look the same from
/// there. So the ordinary case of "that pane isn't there any more" landed the
/// user in the failure state meant for "your machine is unreachable", and
/// `start_pane_spawn`'s fall back to a fresh pane — the whole reason a stale id
/// is survivable — never ran.
///
/// The daemon does answer, it just answers out of band: `Error` on a miss
/// (`daemon::server`), `Size` + `Snapshot` on a hit. Classifying on the **kind
/// byte** rather than the decoded message is what keeps this cheap — the header
/// is 5 bytes and the snapshot behind it can be megabytes.
///
/// Two non-answers are deliberately *not* failures, because neither is evidence
/// the pane is gone and both used to work:
///
/// | | |
/// |---|---|
/// | The read times out | The pane is quiet. Return what we have and let the reader carry on |
/// | Anything but `Error` arrives | It is the replay. Same |
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
                // The daemon hung up without saying anything. Only a `Kill`
                // racing this attach gets here, and the answer is the same one
                // the `Error` frame carries: this pane is not attachable.
                let _ = stream.set_read_timeout(None);
                return Err(anyhow::anyhow!(
                    "the daemon closed the connection without answering Attach for pane {pane_id}"
                ));
            }
            Ok(n) => buffered.extend_from_slice(&scratch[..n]),
            // A timeout leaves the partial frame in `buffered`, where the
            // reader thread resumes it — `take_frame` is written for exactly
            // this.
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
    // An `Error` payload is small and its text is the daemon's own wording for
    // what went wrong, so it is worth finishing the frame to quote it.
    let message = read_error_frame(stream, &mut buffered, wait)
        .unwrap_or_else(|| format!("no such pane {pane_id}"));
    Err(anyhow::anyhow!("daemon refused Attach: {message}"))
}

/// Finish decoding an `Error` frame whose header has already landed in `buffered`.
/// `None` when the rest never arrives — the caller has a serviceable fallback
/// message and no reason to wait around for a better one.
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

/// Whether a read failed because its timeout lapsed rather than because the
/// connection broke. The two platforms disagree on which kind a lapsed
/// `SO_RCVTIMEO` produces, so both count.
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

/// The reset sequence that clears stale full-screen-TUI state from a grid that
/// provably has no full-screen owner (the shell just drew its prompt). Each
/// reset is emitted only when the corresponding mode is actually set, because
/// some are not idempotent when idle: `?1049l` on the primary screen performs
/// a cursor *restore*, so it must never fire as a blanket reset.
///
/// Deliberately left alone: bracketed paste and application cursor keys —
/// zle/fish own those around the prompt and re-arm them on every read, so
/// resetting here could race the line editor's own enable — and anything the
/// parser doesn't track (nothing to detect staleness against).
fn stale_mode_resets(mode: TermMode) -> Vec<u8> {
    let mut seq = Vec::new();
    // Leave the alternate screen first: the resets below then apply to the
    // primary screen's state (kitty keyboard flags are tracked per screen).
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
    // While ALT_SCREEN is set, `mode` shows the *alt* screen's kitty flags;
    // the `?1049l` above restores the primary screen's stack, which may
    // itself be polluted (e.g. a remote kitty-protocol app ran before the
    // TUI that died). So zero the flags whenever either screen could be
    // dirty — at a shell prompt zero is always correct, since kitty-aware
    // line editors re-arm on every read.
    if mode.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL) || mode.contains(TermMode::ALT_SCREEN) {
        seq.extend_from_slice(b"\x1b[=0;1u");
    }
    seq
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
pub(crate) fn notify_desktop(title: Option<&str>, body: &str) {
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
/// isn't a notification we surface. The parsing itself lives in
/// [`crate::core::osc::parse_notification`] (shared with the daemon's agent
/// sniffer); this wrapper additionally drops tty7's own agent-event sentinel —
/// those payloads are machine-to-machine JSON for the daemon's state machine,
/// and toasting them would show raw JSON to the user.
fn parse_osc_notification(payload: &[u8]) -> Option<(Option<String>, String)> {
    if crate::core::cli_agent::parse_agent_event(payload).is_some() {
        return None;
    }
    let (title, body) = crate::core::osc::parse_notification(payload)?;
    // A sentinel-titled payload whose JSON failed to parse is still not a
    // user-facing notification; never toast it.
    if title.as_deref() == Some(crate::core::cli_agent::AGENT_EVENT_SENTINEL) {
        return None;
    }
    Some((title, body))
}

/// Open a fresh connection to the daemon's listening endpoint. The endpoint is
/// resolved through the config dir so it inherits the active `--config-dir`
/// isolation (dev vs. real config dir), exactly like every other config-dir file.
fn connect() -> anyhow::Result<Stream> {
    transport::connect().map_err(|e| {
        // `context`, not a formatted `anyhow!`: callers classify the failure by
        // downcasting to `io::Error` (see `daemon_not_listening`), and
        // interpolating the cause into a string would leave the chain with
        // nothing to find.
        anyhow::Error::new(e).context(format!(
            "connect to daemon at {}",
            transport::endpoint_display()
        ))
    })
}

/// Open a pane connection and, when the pane is a remote workspace's, hand it to
/// the daemon's router before a single `ClientMsg` goes out.
///
/// **A local pane takes the identical path it always did.** `PaneRoute::Local`
/// is `connect()` and nothing else — no extra frame, no extra round trip, no
/// behaviour to regress. Every remote-specific step is inside the `Remote` arm.
///
/// The routed arm blocks for as long as the setup takes, including any question
/// the daemon relays back (a password, install consent). Callers are already on
/// a background thread for the plain `connect()`, and this is the same wait a
/// pane on a cold SSH host has always had.
fn connect_routed(route: &PaneRoute) -> anyhow::Result<Stream> {
    if let PaneRoute::Unroutable(reason) = route {
        return Err(anyhow::anyhow!("{reason}"));
    }
    let Some(header) = route.header() else {
        return connect();
    };

    // Past this point the call blocks on *another computer* — the daemon has to
    // open an SSH channel (doing the whole handshake if nothing is pooled) and
    // the remote `tty7-server` has to answer. The doc above says callers are on
    // a background thread; this is what makes that a rule rather than a hope.
    //
    // The same guard the `Host` trait uses for its filesystem calls, for the
    // same reason and with the same blast radius: `debug_assert!` compiles away
    // in release, so a shipped build never trades a slow pane for a dead app.
    // It fires in development the moment a routed connect is reintroduced on
    // the UI thread — which is how spawning, restoring, listing and killing
    // remote panes each froze the window in turn.
    tty7_core::host::guard_off_ui();

    // WSL installs from the GUI process, never from the daemon: consent has to
    // be raised where it can be answered, and this machine *is* the machine
    // (see `install::wsl::ensure_wsl_server`'s own doc). The daemon's call a
    // moment later finds the binary in place and asks nobody.
    if let crate::daemon::router::RouteTarget::Wsl { distro } = &header.target {
        crate::daemon::install::wsl::ensure_wsl_server(distro)
            .map_err(|e| anyhow::anyhow!("prepare tty7-server in WSL `{distro}`: {e}"))?;
    }

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
        // `alacritty_terminal` leaves this off for embedders by default. tty7's
        // input encoder supports CSI-u, so allow foreground applications to
        // negotiate it instead of collapsing modified keys to legacy bytes.
        kitty_keyboard: true,
        ..Config::default()
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

    // -----------------------------------------------------------------------
    // Routing: a local pane must not change, a remote pane must not be local.
    // -----------------------------------------------------------------------

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

    /// **A local pane writes no extra byte.** The whole compatibility promise of
    /// this milestone in one assertion: `header()` is the only thing that puts a
    /// frame in front of a connection, and a pane with no workspace has none —
    /// so `connect_routed` is a bare `connect()` and the daemon's `handle_conn`
    /// sees the same opening `Spawn` it always did.
    #[test]
    fn a_local_pane_prefixes_nothing() {
        assert!(PaneRoute::Local.header().is_none());
        assert!(PaneRoute::for_workspace(None).header().is_none());
        assert!(matches!(PaneRoute::for_workspace(None), PaneRoute::Local));
        assert!(matches!(PaneRoute::default(), PaneRoute::Local));
    }

    /// A remote workspace's pane routes to its machine, on the **pane** channel.
    ///
    /// The channel is the load-bearing half: a header that defaulted to
    /// `Control` would reach the remote's control socket, where the first
    /// `Spawn` is an unknown frame.
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

    /// WSL routes by distro and carries no spec, because there is no connection
    /// to name.
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

    /// A `--stdio` workspace on this computer routes to a child process and,
    /// crucially, asks it for the **pane** dialect.
    ///
    /// `LocalStdio` runs its argv verbatim — there is no remote shell command
    /// line for the router's `bridge_command` to rewrite — so the `--pane` flag
    /// has to be added here. Without it the pane lands on the control socket
    /// and its first `Spawn` comes back `InvalidData`, which is exactly what
    /// "the window opens but nothing runs in it" looked like.
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

    /// **A workspace that cannot be routed does not fall back to local.**
    ///
    /// Pane ids are per-daemon, so a remote pane whose route is missing must not
    /// address the local daemon: `Kill { pane_id }` there would name a stranger's
    /// pane and succeed.
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

    /// **Only a local pane may make the local daemon restart.**
    ///
    /// `spawn`'s recovery path reads "the connection dropped before the `Spawn`
    /// reply" as a stale local daemon and restarts it — which drains and kills
    /// every pane it hosts. On a routed pane that same symptom means the *far
    /// end* failed while the local daemon was faithfully forwarding bytes, so
    /// acting on it would let one unreachable remote destroy every local
    /// session the user had open. Observed for real: a remote whose
    /// `tty7-server` could not be exec'd took the local daemon down with it.
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

        // Pi and other modern TUIs push their requested progressive-enhancement
        // flags, then query the active mode before deciding how to parse keys.
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

    /// Guards the `alacritty_terminal` pin, not our own code. Upstream's
    /// `push_keyboard_mode` trims its stack by removing from `title_stack` — a
    /// copy-paste slip from `push_title` — so once the title stack is empty the
    /// `Vec::remove(0)` panics and takes the reader thread with it. Enabling
    /// `kitty_keyboard` made that reachable from any foreground program: ~20KB of
    /// unpopped pushes is enough. Our fork fixes it; a bump back to an unpatched
    /// rev must fail here rather than in the field.
    #[test]
    fn deep_keyboard_mode_pushes_leave_the_reader_alive() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // One past alacritty's KEYBOARD_MODE_STACK_MAX_DEPTH (4096): the push that
        // overflows is the one that used to panic. The trailing query is the
        // liveness probe — a dead reader thread simply never answers.
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

    /// Guards the `alacritty_terminal` pin, not our own code. Upstream reserves
    /// columns one `char` at a time, so an emoji written as base + `U+FE0F`
    /// (`❤️`, `🗂️`, `⚠️` — anything whose base is East Asian Width Neutral) gets
    /// one column instead of two and shoves the rest of the line left by one.
    /// Our fork re-scores the sequence and widens the cell; a bump back to an
    /// unpatched rev must fail here rather than in the field (issue #203).
    #[test]
    fn emoji_presentation_sequences_reserve_two_columns() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // `x` marks where the emoji ended: column 2 if ❤️ got its two columns,
        // column 1 if the selector was counted as free.
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

    /// A dead daemon is the one failure the client can fix by itself, and it
    /// must be told apart from a live daemon saying no — restarting on *that*
    /// would kill every running pane over a bad shell setting.
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

        // A daemon that answered and refused, and one that hung up mid-Spawn:
        // neither is "not running", and each has its own recovery.
        let refused = anyhow::anyhow!("daemon refused Spawn: configured shell missing");
        assert!(!daemon_not_listening(&refused));
        let eof: anyhow::Error =
            std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "closed").into();
        assert!(!daemon_not_listening(&eof));
    }

    // -----------------------------------------------------------------------
    // Attach: telling "that pane is gone" from "that pane is quiet".
    // -----------------------------------------------------------------------

    /// **A pane that is gone makes the attach fail.** The regression this
    /// exists for: `Attach` has no synchronous reply, so the client used to
    /// return `Ok` unconditionally and the daemon's `Error` frame was read much
    /// later by the reader thread, which has no arm for it — the socket then
    /// closed and the pane landed in the *link is down* state (`tty7 —
    /// disconnected`, kept on screen, never respawned) instead of falling back
    /// to a fresh shell in `start_pane_spawn`. Ending a workspace's sessions
    /// and reopening it hit exactly this.
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

    /// A daemon that hangs up without answering is the same answer by other
    /// means — a `Kill` racing the attach closes the connection.
    #[test]
    fn an_attach_the_daemon_hangs_up_on_is_an_error() {
        let (mut client_side, daemon_side) = UnixStream::pair().unwrap();
        drop(daemon_side);
        assert!(
            attach_reply_prefix(&mut client_side, 7, attach_reply_wait(&PaneRoute::Local)).is_err()
        );
    }

    /// **A local attach's wait is bounded by the UI, not by the network.** It
    /// runs synchronously on the UI thread (`ui::pending_pane` explains why),
    /// so the wait for the daemon's first frame is a possible window freeze;
    /// the remote one is on a background thread and can be patient. Equal
    /// numbers here would mean a wedged local daemon freezing restore for
    /// fifteen seconds per pane.
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

    /// **The bytes read to classify the reply are not consumed.** A successful
    /// attach's first frame is the head of the replay, so anything the check
    /// pulled off the socket has to reach the reader thread — losing it would
    /// mean reopening a workspace to a screen missing its first segment.
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

        // DECSCUSR 6 = steady beam, the sequence nvim uses for insert mode.
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

        // DECSCUSR 0 clears the application override, so the configured
        // terminal default is visible again.
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

    /// Native-SSH `AuthPrompt` and `SshStatus` frames must surface through the
    /// reader thread into the per-pane queue / phase cell the auth sheet reads.
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

        // Poll until the prompt lands (reader applies frames asynchronously).
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
        // The queue is now drained.
        assert!(!term.has_pending_auth());
    }

    /// A `DaemonMsg::Exited` frame (the child really ended) must set
    /// `child_exited`; a bare daemon disconnect (EOF) must not — both flip
    /// `exited_flag`. The distinction is what keeps pane auto-close from
    /// firing on a lost connection and destroying a session that may still be
    /// alive daemon-side.
    #[test]
    fn child_exit_is_distinguished_from_daemon_disconnect() {
        // A genuine child exit: the daemon reports it explicitly.
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

        // A daemon disconnect: the socket just closes.
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

    /// `stale_mode_resets` maps each residue bit to its reset — and nothing
    /// more. The guards matter as much as the resets: `?1049l` on a grid that
    /// is *not* on the alt screen performs a cursor restore, so a clean (or
    /// merely cursor-hidden) mode must never emit it.
    #[test]
    fn stale_mode_resets_target_only_the_dirty_bits() {
        // A healthy prompt-time mode: nothing to reset.
        let clean = TermMode::SHOW_CURSOR | TermMode::LINE_WRAP | TermMode::BRACKETED_PASTE;
        assert!(stale_mode_resets(clean).is_empty());

        // Hidden cursor alone (a Claude-Code-style TUI, no alt screen):
        // exactly `?25h`, and crucially no `?1049l`.
        let hidden = TermMode::LINE_WRAP;
        assert_eq!(stale_mode_resets(hidden), b"\x1b[?25h");

        // The full ssh-drop-mid-htop residue: alt screen + hidden cursor +
        // mouse reporting. The alt-screen exit leads (later resets must land
        // on the primary screen), and the kitty zeroing rides along because
        // the primary screen's flags are unobservable from the alt screen.
        let residue = TermMode::ALT_SCREEN | TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
        let seq = stale_mode_resets(residue);
        let text = String::from_utf8_lossy(&seq).into_owned();
        assert!(text.starts_with("\x1b[?1049l"));
        assert!(text.contains("\x1b[?25h"));
        assert!(text.contains("\x1b[?1002l"));
        assert!(text.contains("\x1b[?1006l"));
        assert!(text.ends_with("\x1b[=0;1u"));

        // Kitty keyboard flags alone (the same drop during a kitty-protocol
        // app): just the zeroing, nothing screen-related.
        let kitty = TermMode::SHOW_CURSOR | TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(stale_mode_resets(kitty), b"\x1b[=0;1u");
    }

    /// End-to-end through the reader thread: a TUI's mode changes arrive as
    /// `Output`, the connection "dies" (no restore sequences), and the host
    /// shell's next prompt report must scrub the residue from the local grid.
    /// This is the ssh-drop-mid-TUI bug at the transport level.
    #[test]
    fn prompt_report_scrubs_stale_tui_modes() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();

        // htop over ssh: alt screen, hidden cursor, drag + SGR mouse. Then the
        // network drops — no `?1049l`/`?25h`/mouse-off ever arrives.
        DaemonMsg::Output(b"\x1b[?1049h\x1b[?25l\x1b[?1002h\x1b[?1006h".to_vec())
            .encode(&mut daemon_side)
            .unwrap();
        // ssh exits; the host shell's integration reports a fresh prompt.
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

    /// `foreground_agent` reflects the daemon's last `Agent` report — `None`
    /// before any report, the detected agent after one, and back to `None` when
    /// the agent exits (the daemon reports `Agent(None)`).
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

    /// `DaemonMsg::AgentStatus` frames must land in the client's session
    /// cache (and a `None` clear it) — the reader half of the rich-status
    /// channel the daemon's sniffer feeds.
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

    /// End-to-end check of the Outline's data path: OSC 133 marks arriving in the
    /// output stream must land in `Marks` with the *grid row they fell on*, not
    /// the row at the end of the batch. This is the whole reason the reader
    /// splits its advance at mark offsets, so it's worth an integration test —
    /// a regression here looks fine (marks appear) but scrolls to the wrong place.
    #[test]
    fn marks_record_the_row_each_one_landed_on() {
        let (client_side, mut daemon_side) = UnixStream::pair().unwrap();
        let term = RemoteTerminal::from_stream(client_side, TermSize::new(80, 24)).unwrap();
        let poll = |want: usize| {
            for _ in 0..200 {
                if term.marks().list().len() == want {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            false
        };

        // Two full prompt cycles in ONE batch, separated by output lines. If the
        // reader advanced the batch in a single pass and read the cursor after,
        // both marks would report the same (final) row.
        let mut stream = Vec::new();
        stream.extend_from_slice(b"\x1b]133;A\x07"); // prompt 1 at row 0
        stream.extend_from_slice(b"\x1b]133;C;echo one\x07");
        stream.extend_from_slice(b"one\r\n");
        stream.extend_from_slice(b"\x1b]133;D;0\x07");
        stream.extend_from_slice(b"\x1b]133;A\x07"); // prompt 2, two rows down
        stream.extend_from_slice(b"\x1b]133;C;false\x07");
        stream.extend_from_slice(b"\r\n");
        stream.extend_from_slice(b"\x1b]133;D;1\x07");
        DaemonMsg::Output(stream).encode(&mut daemon_side).unwrap();
        daemon_side.flush().unwrap();

        assert!(poll(2), "both commands recorded");
        let marks = term.marks().list();
        assert_eq!(marks[0].text, "echo one");
        assert_eq!(marks[0].exit, Some(0));
        assert_eq!(marks[1].text, "false");
        assert_eq!(marks[1].exit, Some(1), "a failure keeps its exit code");
        assert!(
            marks[1].row > marks[0].row,
            "the second prompt is further down the scrollback ({} vs {}) — equal rows \
             would mean the advance wasn't split at the marks",
            marks[0].row,
            marks[1].row
        );
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

    /// Everything in the grid — screen rows plus scrollback — flattened to one
    /// string, one row per line, for substring counting in the replay test.
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

    /// One Claude-Code/ink-style redraw: return to the frame's first row with
    /// CR + cursor-up, erase below, reprint every line. `prev_rows` is the row
    /// count the *app* believes the previous frame occupied — correct only if
    /// the terminal wrapped it at the width the app rendered for.
    fn tui_frame(lines: &[String], prev_rows: usize) -> Vec<u8> {
        let mut b = Vec::new();
        if prev_rows > 1 {
            b.extend_from_slice(format!("\r\x1b[{}A\x1b[J", prev_rows - 1).as_bytes());
        }
        b.extend_from_slice(lines.join("\r\n").as_bytes());
        b
    }

    /// Regression for the "Claude Code output duplicated all over scrollback
    /// after restart" bug. The daemon's replay ring is raw bytes; a TUI's
    /// cursor-up redraws only replay cleanly at the width they were rendered
    /// for. The daemon therefore segments the ring by geometry and attach
    /// replays a `Size` → `Snapshot` pair per segment (see
    /// `daemon/pane.rs::ReplayRing`) — this test drives the reader with
    /// exactly that frame sequence and asserts the replay reproduces the live
    /// rendering, no duplication. The final leg replays the same bytes the
    /// pre-segmentation way (one Snapshot at the final width) and shows the
    /// duplication, pinning that the segmented path is what prevents it.
    #[test]
    fn segmented_ring_replay_reproduces_live_rendering() {
        const MARK: &str = "DUPMARK";
        // 10 logical lines of 90 chars: one row on a 100-col grid, two on 80.
        let frame_lines = |f: usize| -> Vec<String> {
            (0..10)
                .map(|i| format!("{MARK} f{f:02} l{i:02} {:.<74}", ""))
                .collect()
        };
        // The app renders 8 frames believing each line is one row (true at the
        // 100-col width it was written for).
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

        // Live: the bytes stream into a 100-col grid as PTY output, then the
        // pane shrinks to 80 (reflow) — the sequence the ring recorded.
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

        // Attach replay, as the daemon now sends it: the 100-col segment at
        // its recorded width, then the (empty) post-resize segment's pair
        // ending the grid at the current 80 cols.
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

        // Contrast (and guard that the markers actually exercise the wrap
        // hazard): the pre-segmentation replay — everything in one Snapshot at
        // the final 80-col width — mis-wraps the frames, the redraws land
        // mid-frame, and stale copies flood scrollback.
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

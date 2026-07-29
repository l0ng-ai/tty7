//! The window's half of "Connect to Host".
//!
//! [`ui::remote_connect`](crate::ui::remote_connect) is the plumbing — SSH
//! specs, routed control connections, the remote workspace store. This is the
//! part that lives on a window: the state the home page renders, the steps that
//! move between those states, and the guards that keep a window on one machine.
//!
//! ## One window, one machine
//!
//! There is a single rule, and its inverse is listed under *never do
//! this*: **a window shows one workspace on one machine, and every tab and pane
//! in it is on that machine.** The whole M5 data layer leans on it — a workspace
//! stores its `host` once rather than per pane, and `sidebar_group` stays a bare
//! `PathBuf` because a repo root only has to be unique within one machine.
//!
//! So the invariant cannot be a comment. Three things enforce it here:
//!
//! | Path | Guard |
//! |---|---|
//! | New tab / split | [`Tty7App::spawn_host`] — a remote window refuses to spawn a local shell |
//! | Reopening a closed tab | [`Tty7App::rebind_host`] clears the closed stack when a window changes machine |
//! | Restart / session restore | `WorkspaceStore::record`'s storage split — a remote entry never holds a local layout on disk |
//!
//! The fourth path, dragging a tab between windows, does not exist in tty7:
//! tabs never leave the window they were opened in, so there is nothing to
//! guard there. If that ever changes, it needs a fourth row.
//!
//! ## What is left for M6
//!
//! The flow below reaches `Attached` and stops. Reconnect backoff, takeover
//! (`Preempted`) and the start-up auth queue are M6, and the
//! seams for them are named where they belong: [`RemoteStatus`] has the two
//! states to add, [`Tty7App::connect_remote_workspace`] is the one entry point
//! that opens a connection, and [`Tty7App::reopen_remote_at_startup`] is where
//! the "`open: true` remote workspaces reconnect at launch" rule hooks in.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{Context, PromptLevel, Window};
use gpui_component::WindowExt as _;
use tty7_core::host::HostId;
use tty7_core::host::remote::RemoteHost;

use crate::core::session::{RemoteRef, RemoteTarget, WorkspaceId, WorkspaceStore};
use crate::daemon::control::{ControlEvent, ControlRequest, ReplyOk};
use crate::daemon::install::InstallDecision;
use crate::ui::app::Tty7App;
use crate::ui::remote_connect::{self, HostChoice, RemoteWorkspaceRow};

/// Where the home page's "Connect to Host" flow has got to.
///
/// Every state is a place the user can be *left*: no
/// failure closes a window, so [`ConnectFlow::Failed`] is a resting state with
/// its own affordances rather than a toast on the way back to the picker.
pub enum ConnectFlow {
    /// Reaching a machine. Blocking work is on a background task; this is what
    /// the panel shows while it runs.
    Connecting { choice: HostChoice },
    /// It did not work, and the window stays here until the user decides what to
    /// do about it (never auto-close, always offer the next move).
    Failed { choice: HostChoice, error: String },
}

impl ConnectFlow {
    /// The machine this flow is about.
    ///
    /// Still an `Option` at the call sites because the *flow* is optional; both
    /// remaining states name a machine, which is why picking one no longer has a
    /// state of its own — [`ui::switcher`](crate::ui::switcher) is the list now.
    pub fn choice(&self) -> Option<&HostChoice> {
        match self {
            ConnectFlow::Connecting { choice } | ConnectFlow::Failed { choice, .. } => Some(choice),
        }
    }
}

/// A **remote** workspace's connection state.
///
/// Only remote workspaces have one — a local window is not a disconnected
/// remote window, it is a window with no machine to be connected to, which is
/// why [`Tty7App::remote_status`] answers `Option`. Without that the enum grows
/// a "local" variant that every `match` then has to pretend is a network state.
///
/// All six states. `Reconnecting` and `Preempted` were named here by M5 before
/// they existed, because every consumer — the status strip, the read-only
/// degrade, the input gate — wants to switch on the whole set, and a
/// two-variant enum would get written as a `bool` instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteStatus {
    /// There is no connection and nothing is trying to make one.
    Disconnected,
    Connecting,
    Attached,
    /// The link dropped and the supervisor is retrying, for ever, on the
    /// backoff in [`Backoff`]. Read-only, and the window stays open — design
    /// No failure closes a window.
    Reconnecting {
        /// How many attempts have already failed. Shown because "reconnecting…"
        /// that has been on screen for four minutes should say so.
        attempt: u32,
    },
    /// Somebody else attached to this workspace (D8). Read-only and
    /// **not** retried: a client that reconnected automatically would fight the
    /// machine the user just moved to. Taking it back is a deliberate act.
    Preempted {
        /// The machine that took it, as the remote reported it.
        by: String,
    },
    /// An attempt failed, with the reason. Distinct from `Disconnected` because
    /// the reason is the most useful thing on the screen, and because only this
    /// state has something to retry.
    Failed(String),
}

impl RemoteStatus {
    /// The one-line status the window shows. `None` only when everything is
    /// working — a permanent "you are fine" banner is noise.
    pub fn strip_message(&self, machine: &str) -> Option<String> {
        match self {
            RemoteStatus::Attached => None,
            RemoteStatus::Disconnected => Some(format!("Not connected to {machine}")),
            RemoteStatus::Connecting => Some(format!("Connecting to {machine}…")),
            RemoteStatus::Reconnecting { attempt: 0 } => {
                Some(format!("Reconnecting to {machine}…"))
            }
            RemoteStatus::Reconnecting { attempt } => Some(format!(
                "Reconnecting to {machine}… (attempt {})",
                attempt + 1
            )),
            RemoteStatus::Preempted { by } => Some(format!("This workspace was opened on {by}")),
            RemoteStatus::Failed(e) => Some(format!("Not connected to {machine} — {e}")),
        }
    }

    /// The line along the bottom of a degraded window (底部一条
    /// "未连接 — 输入暂不生效").
    ///
    /// Separate from [`RemoteStatus::strip_message`] because they answer
    /// different questions and sit at opposite ends of the window: the strip
    /// says *what is happening to the connection*, this says *what that means
    /// for the keyboard*. Collapsing them would put a retry countdown next to
    /// the cursor or a typing notice in the title area.
    pub fn input_notice(&self) -> Option<&'static str> {
        match self {
            RemoteStatus::Attached => None,
            RemoteStatus::Preempted { .. } => Some("Opened elsewhere — typing has no effect"),
            _ => Some("Not connected — typing has no effect"),
        }
    }

    /// What the status strip's button offers, or `None` when there is nothing
    /// useful to do: a failure state always offers the next move.
    pub fn action_label(&self) -> Option<&'static str> {
        match self {
            RemoteStatus::Attached | RemoteStatus::Connecting => None,
            // Retrying *now* rather than waiting out the backoff. The automatic
            // retry keeps running either way, so this can only ever help.
            RemoteStatus::Reconnecting { .. } => Some("Retry Now"),
            RemoteStatus::Preempted { .. } => Some("Take Back"),
            RemoteStatus::Disconnected => Some("Connect"),
            RemoteStatus::Failed(_) => Some("Retry"),
        }
    }

    /// Whether keystrokes reach the panes. The read-only degrade: a
    /// window that is not attached still scrolls, selects, copies and searches,
    /// but typing goes nowhere and is **not** buffered (D6).
    ///
    /// The rule lives here rather than in the pane's key handler because the
    /// decision is about the *workspace's* connection, not about a pane — and
    /// because a rule stated once cannot disagree with itself across the five
    /// places a keystroke can enter a pane (`on_key_down`, the IME's
    /// `commit_text`, `paste`, `send_to_pty`, and the typeahead `dump_hold`
    /// timer). [`workspace_accepts_input`] is the form those call sites use.
    #[allow(
        dead_code,
        reason = "reached through `workspace_accepts_input`, whose callers are in terminal/view.rs"
    )]
    pub fn accepts_input(&self) -> bool {
        matches!(self, RemoteStatus::Attached)
    }
}

// ---------------------------------------------------------------------------
// Reconnect backoff (指数退避 1/2/4/…/30s 封顶，无限重试)
// ---------------------------------------------------------------------------

/// The first wait after a link drops.
pub const RECONNECT_FIRST: std::time::Duration = std::time::Duration::from_secs(1);
/// The ceiling the doubling stops at, fixed at 30s: long enough
/// that a machine that has been down for an hour is not being probed every
/// second, short enough that plugging the cable back in feels immediate.
pub const RECONNECT_CAP: std::time::Duration = std::time::Duration::from_secs(30);

/// The retry schedule: 1, 2, 4, 8, 16, 30, 30, … and **never gives
/// up**.
///
/// Giving up is the one thing this must not do. The window stays open in a
/// read-only state either way, so a supervisor that stopped retrying would
/// leave the user looking at a dead window whose only cure is a button they
/// have no reason to think exists. Retrying at 30s costs one connect attempt
/// every half minute, which is nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    /// How many attempts have already failed.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// How long to wait before the next attempt, without consuming it.
    pub fn delay(&self) -> std::time::Duration {
        // `checked_shl` rather than `1 << n`: past 63 the shift is undefined
        // behaviour in C and a panic in debug Rust, and an infinite retry loop
        // reaches any exponent you care to name if it is left running long
        // enough. Saturating into the cap is the answer at every exponent.
        let secs = 1u64.checked_shl(self.attempt).unwrap_or(u64::MAX);
        RECONNECT_FIRST
            .saturating_mul(u32::try_from(secs).unwrap_or(u32::MAX))
            .min(RECONNECT_CAP)
    }

    /// Take the next delay and count the attempt.
    pub fn advance(&mut self) -> std::time::Duration {
        let delay = self.delay();
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    /// Back to the start, after a connection succeeds.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

// ---------------------------------------------------------------------------
// The start-up auth queue (D7)
// ---------------------------------------------------------------------------

/// One auth sheet at a time per window; the rest queue.
///
/// **It queues sheets, not connections.** D7 wants machines that need no
/// interaction (a key, `ssh-agent`, a connection already authenticated) to
/// connect *in parallel*, so nothing here is consulted until a connect actually
/// asks a human something. A design that classified hosts up front would have to
/// guess, and would guess differently on two machines with the same config —
/// the exact failure D7 rejects.
///
/// The contract for whoever routes prompts (the daemon's `PromptBroker` relay):
///
/// | Step | Call |
/// |---|---|
/// | A connect needs a password / passphrase / host-key answer | [`AuthSheetQueue::request`] |
/// | It answered `true` | Show the sheet |
/// | It answered `false` | Park the request; the queue will name this host from `release` |
/// | The sheet is answered, cancelled or its connect died | [`AuthSheetQueue::release`] |
/// | The connect gave up before its turn came | [`AuthSheetQueue::withdraw`] |
///
/// Keyed by [`HostId`] rather than by window: the credential is the machine's,
/// two windows on one box share a connection, and asking twice for one password
/// is the behaviour this exists to prevent.
#[derive(Default)]
// The consumers are the daemon's prompt relay and the sheet it raises, both of
// which land with the routed auth path — this is the queue they call into, and
// the contract table above is what they implement against. Dropped here rather
// than left unused elsewhere so the rule has exactly one home.
#[allow(
    dead_code,
    reason = "the prompt relay that calls this is the other half of D7's start-up connect"
)]
pub struct AuthSheetQueue {
    holder: Option<HostId>,
    waiting: std::collections::VecDeque<HostId>,
}

#[allow(
    dead_code,
    reason = "the prompt relay that calls this is the other half of D7's start-up connect"
)]
impl AuthSheetQueue {
    /// Ask to raise a sheet for `who`. `true` means show it now.
    ///
    /// Re-asking while already holding it answers `true` again: a connect that
    /// asks twice (a key passphrase, then the host key) must not deadlock behind
    /// itself.
    pub fn request(&mut self, who: HostId) -> bool {
        match self.holder {
            Some(current) if current == who => true,
            Some(_) => {
                if !self.waiting.contains(&who) {
                    self.waiting.push_back(who);
                }
                false
            }
            None => {
                self.holder = Some(who);
                self.waiting.retain(|w| *w != who);
                true
            }
        }
    }

    /// The sheet for `who` is done with. Answers whoever may go next.
    ///
    /// A release from a host that is *not* the holder is ignored rather than
    /// treated as an error — a connect that failed on its own can call this
    /// without first checking whether it ever got the sheet.
    pub fn release(&mut self, who: HostId) -> Option<HostId> {
        if self.holder != Some(who) {
            self.waiting.retain(|w| *w != who);
            return None;
        }
        self.holder = self.waiting.pop_front();
        self.holder
    }

    /// Give up a place in the queue without ever having held it.
    pub fn withdraw(&mut self, who: HostId) {
        self.waiting.retain(|w| *w != who);
    }

    /// Who may show a sheet right now.
    pub fn holder(&self) -> Option<HostId> {
        self.holder
    }

    /// How many are queued behind the current sheet.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }
}

impl Tty7App {
    // ----- the invariant ---------------------------------------------------

    /// The machine this window's panes must be on.
    ///
    /// Derived from the workspace rather than stored, so it cannot drift: a
    /// window's host *is* its workspace's host, and rebinding the window to
    /// another workspace changes it by construction.
    pub(crate) fn spawn_host(&self, cx: &gpui::App) -> HostId {
        WorkspaceStore::host_of(cx, self.workspace)
    }

    /// Whether this window may open a shell on *this* machine.
    ///
    /// The guard on that "never do this". A remote window that spawned a
    /// local shell would put two machines in one window — and would do it
    /// invisibly, because a local shell in a remote window looks exactly like a
    /// remote one until the first `ls`.
    pub(crate) fn can_spawn_locally(&self, cx: &gpui::App) -> bool {
        self.spawn_host(cx).is_local()
    }

    /// Refuse a spawn this window cannot route, saying why. Returns `true` when
    /// the caller may go ahead.
    ///
    /// **A remote window is no longer refused.** Its panes take
    /// [`Tty7App::window_workspace`]'s route to the machine the window is bound
    /// to, so "+" there opens a shell *over there*, which is the whole feature.
    /// What is still refused is a remote window whose machine has no address on
    /// file — a deleted profile, an alias gone from `~/.ssh/config`. Falling
    /// back to a local shell for those would put two machines in one window and
    /// do it invisibly, because a local shell in a remote window looks exactly
    /// like a remote one until the first `ls`.
    pub(crate) fn guard_local_spawn(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.can_spawn_locally(cx) {
            return true;
        }
        match self.window_workspace(cx) {
            // Routable: this is a remote window doing exactly what it is for.
            Some(ws) if ws.route_header().is_ok() => true,
            _ => {
                let machine = self.remote_machine_label(cx);
                window.push_notification(
                    format!(
                        "This window is a workspace on {machine}, but tty7 has no connection \
                         details for it any more — check that its SSH profile or ~/.ssh/config \
                         entry still exists."
                    ),
                    cx,
                );
                false
            }
        }
    }

    /// What this window's panes carry so their connections reach its machine.
    ///
    /// `None` for a local window, which is the whole of how a local pane stays
    /// byte-for-byte what it always was: `PaneRoute::for_workspace(None)` is
    /// `Local`, and `Local` writes no header.
    pub(crate) fn window_workspace(
        &self,
        cx: &gpui::App,
    ) -> Option<crate::terminal::PaneWorkspace> {
        pane_workspace_for(cx, self.workspace)
    }

    /// The user-facing name of the machine this window is bound to, or
    /// `"this computer"` for a local window.
    pub(crate) fn remote_machine_label(&self, cx: &gpui::App) -> String {
        match WorkspaceStore::remote_ref(cx, self.workspace) {
            Some(host) => host.target.to_string(),
            None => "this computer".to_string(),
        }
    }

    /// React to this window changing which machine it shows.
    ///
    /// The closed-tab stack is the one piece of per-*window* state that outlives
    /// a workspace swap, so it is the one thing that could carry a tab across
    /// machines: reopen it after switching from a local workspace to a remote
    /// one and a local shell lands in a remote window. Dropping the stack on a
    /// host change is a small loss (⌘⇧T stops working once, right after a
    /// deliberate switch) against the invariant it protects.
    pub(crate) fn rebind_host(&mut self, previous: HostId, cx: &gpui::App) {
        if crate::core::session::crosses_machines(previous, self.spawn_host(cx)) {
            self.closed.clear();
        }
    }

    /// The short name of the shell a plain new tab on this window's machine
    /// lands on — the menu's `default` tag, and the details panel's "shell" row
    /// for a pane that never named one.
    ///
    /// A local window reads the live `Config` global rather than what the probe
    /// recorded, so changing `shell` in Settings retags the menu at once. A
    /// remote window's default is a fact about the far machine — its own
    /// `config.json`, its own `$SHELL` — and only it can report it.
    pub(crate) fn default_shell_label(&self, cx: &gpui::App) -> String {
        if self.shells_host.is_local() {
            crate::core::shells::default_shell_name(
                cx.global::<crate::core::config::Config>()
                    .shell
                    .as_ref()
                    .map(|s| s.program.as_str()),
            )
        } else {
            self.shells.default_name.clone()
        }
    }

    /// Refill the "+" dropdown from the machine this window is bound to.
    ///
    /// The fourth row of the module header's table, in effect: a window that
    /// listed *this* computer's shells would hand a remote spawn `/bin/zsh` on a
    /// box whose zsh is `/usr/bin/zsh`, and the pane would come up as a spawn
    /// failure rather than a shell. So the list is a property of the window's
    /// machine, refetched whenever that machine changes or comes back.
    ///
    /// A machine that isn't reachable yet empties the list rather than keeping
    /// the last one: the window is either still connecting (the connect calls
    /// back here) or offline, and in both cases a stale menu would be offering
    /// picks that cannot be spawned. The dropdown falls back to its plain "New
    /// Tab" entry, which the far end resolves with its own default shell.
    pub(crate) fn refresh_shells(&mut self, cx: &mut Context<Self>) {
        let host_id = self.spawn_host(cx);
        self.shells_host = host_id;
        let Some(host) = crate::ui::host_registry::HostRegistry::get(cx, host_id) else {
            self.shells = Default::default();
            cx.notify();
            return;
        };
        // Off the UI thread: `/etc/shells` plus a `PATH` walk locally, a round
        // trip (and a `wsl.exe` spawn on a Windows peer) remotely.
        crate::ui::host_ops::HostOps::run(
            host,
            cx,
            |h| h.shells(),
            move |app, out, cx| {
                // The window may have moved to another machine while this was in
                // the air; a late answer for the machine it left is not an answer
                // about the one it is on.
                if app.shells_host != host_id {
                    return;
                }
                app.shells = match out {
                    Ok(inventory) => inventory,
                    Err(e) => {
                        log::warn!("could not list the shells of this window's machine: {e}");
                        Default::default()
                    }
                };
                // Nothing else redraws an idle window, and the dropdown's
                // closure captures the list at build time.
                cx.notify();
            },
        );
    }

    /// Every pane in this window, in tab order.
    ///
    /// The reconnect walks these: a workspace's panes are exactly the panes of
    /// the one window showing it (one window, one workspace, one
    /// machine), so there is no second place to look.
    pub(crate) fn panes(&self) -> Vec<gpui::Entity<crate::terminal::view::TerminalView>> {
        self.tabs
            .iter()
            .flat_map(|tab| tab.pane.terminals())
            .collect()
    }

    /// This window's connection state.
    ///
    /// Two sources, in the order that matters. The *picker* flow wins while it
    /// is running — a window mid-connect is showing that connect, not the
    /// machine's background link — and everything else comes from the
    /// supervisor ([`RemoteLinks`]), which is where `Reconnecting` and
    /// `Preempted` live because they are events, not derivable facts.
    pub(crate) fn remote_status(&self, cx: &gpui::App) -> Option<RemoteStatus> {
        WorkspaceStore::remote_ref(cx, self.workspace)?;
        match &self.connect {
            Some(ConnectFlow::Connecting { .. }) => return Some(RemoteStatus::Connecting),
            Some(ConnectFlow::Failed { error, .. }) => {
                return Some(RemoteStatus::Failed(error.clone()));
            }
            _ => {}
        }
        RemoteLinks::status_of(cx, self.workspace)
    }

    /// The status strip's button ([重试] / [抢回]).
    pub(crate) fn remote_retry(&mut self, cx: &mut Context<Self>) {
        match &self.connect {
            // Mid-picker: the retry the user can see is the picker's own.
            Some(ConnectFlow::Failed { choice, .. }) => {
                let choice = choice.clone();
                self.connect_to_host(choice, cx);
            }
            _ => RemoteLinks::retry_now(cx, self.workspace),
        }
        cx.notify();
    }

    // ----- the flow ---------------------------------------------------------

    /// Reach `choice` and, on success, show what workspaces it has.
    ///
    /// The blocking half runs on the background executor — it opens sockets,
    /// authenticates and waits on a remote — and the `Host` layer actively
    /// refuses to be called from the UI thread, which is the check that keeps
    /// this honest.
    pub(crate) fn connect_to_host(&mut self, choice: HostChoice, cx: &mut Context<Self>) {
        remote_connect::register(cx);
        let header = match remote_connect::control_route(&choice.target, cx) {
            Ok(header) => header,
            Err(e) => {
                self.connect = Some(ConnectFlow::Failed { choice, error: e });
                cx.notify();
                return;
            }
        };
        let target = choice.target.clone();
        let label = choice.label.clone();
        // Connecting is the answer to having disconnected, so it clears it —
        // otherwise the supervisor would tear this connection back down on its
        // next tick, and the user would have pressed Connect to no effect.
        cx.default_global::<RemoteLinks>()
            .suspended
            .remove(&choice.target.host_id());
        self.connect = Some(ConnectFlow::Connecting {
            choice: choice.clone(),
        });
        cx.notify();

        // A retry after a failed install must not paint the previous attempt's
        // bar for the instant before the first new report lands.
        remote_connect::clear_install_progress(choice.target.host_id());
        self.watch_for_install_consent(choice.target.host_id(), cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { remote_connect::connect_blocking(&target, header, &label) })
                .await;
            let _ = this.update(cx, |this, cx| this.finish_connect(result, cx));
        })
        .detach();
    }

    /// Watch for an install that is blocked on the user's consent while a
    /// connect is in flight.
    ///
    /// A separate loop rather than a step in the connect task because the
    /// installer *blocks* on the answer: the request appears while the connect
    /// future is still pending, so anything that only looked after it finished
    /// would deadlock until the consent timeout. The loop ends when the flow
    /// leaves `Connecting`, so it costs nothing when nothing is connecting.
    ///
    /// It is also what paints the install's progress bar. The bytes arrive far
    /// faster than a screen refresh — hundreds of reports over one install — so
    /// they land in a slot ([`remote_connect::GuiInstallProgress`]) and this
    /// loop samples it, repainting only when the number it reads has actually
    /// changed. A connect that installs nothing therefore costs no repaints at
    /// all.
    fn watch_for_install_consent(&self, host: HostId, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut painted: Option<crate::daemon::install::InstallPhase> = None;
            loop {
                let connecting = this
                    .update(cx, |this, _| {
                        matches!(this.connect, Some(ConnectFlow::Connecting { .. }))
                    })
                    .unwrap_or(false);
                if !connecting {
                    return;
                }
                if let Some(pending) = remote_connect::take_pending_install() {
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.prompt_install_consent(pending, window, cx)
                    });
                }
                let reported = remote_connect::install_progress_for(host);
                if reported != painted {
                    painted = reported;
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
                // The other question a routed connect can raise: a password, a
                // key passphrase, a host key. Same mailbox shape, same reason it
                // needs one (the daemon holds the connection, this process holds
                // the user), and the same 100 ms poll.
                cx.update(pump_auth_sheets);
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
            }
        })
        .detach();
    }

    /// Land a finished connect attempt in the flow's state.
    fn finish_connect(
        &mut self,
        result: Result<remote_connect::Connected, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(choice) = self.connect.as_ref().and_then(ConnectFlow::choice).cloned() else {
            // The user left the flow while it was in the air. The connection
            // drops with the result; nothing to show.
            return;
        };
        // Either way the install is over: on success there is nothing left to
        // report, and on failure the error needs the space the bar was in.
        remote_connect::clear_install_progress(choice.target.host_id());
        match result {
            Ok(connected) => {
                let home = connected.home.clone();
                let rows = connected.rows.clone();
                // The switcher lists every machine at once, so a handshake's
                // answer has to outlive the flow that fetched it: the flow holds
                // one machine and forgets it on the next connect.
                self.host_snapshots.insert(
                    choice.target.host_id(),
                    crate::ui::switcher::HostSnapshot {
                        target: choice.target.clone(),
                        rows: rows.clone(),
                    },
                );
                remote_connect::RemoteConnections::insert(cx, connected.host, home.clone());
                self.prompt_remote_daemon_mismatch_later(cx);
                // Nothing left to *show* about the attempt: the machine is now
                // in `RemoteConnections` and its group in the switcher fills
                // itself from there and from the snapshot above.
                self.connect = None;
            }
            Err(error) => {
                // This is a resting state. The window keeps everything it
                // had, says what went wrong, and offers the next move.
                log::warn!("connect to {} failed: {error}", choice.label);
                self.connect = Some(ConnectFlow::Failed { choice, error });
            }
        }
        cx.notify();
    }

    /// Open a workspace that already exists on the connected machine.
    pub(crate) fn open_remote_workspace(
        &mut self,
        target: RemoteTarget,
        row: RemoteWorkspaceRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = RemoteRef::new(target, row.id);
        let id = WorkspaceStore::claim_remote(cx, host);
        WorkspaceStore::apply_remote(cx, id, &row.record);
        self.enter_remote_workspace(id, window, cx);
    }

    /// Make a workspace on the connected machine, rooted at *its* `$HOME`.
    ///
    /// A new workspace lands in `~` — the remote's, taken from the
    /// control handshake (`ControlHelloOk.home`), never this client's. A window
    /// on a Mac opening a workspace on a Linux box must land in
    /// `/home/<them>`, not `/Users/<me>`.
    pub(crate) fn create_remote_workspace(
        &mut self,
        target: RemoteTarget,
        home: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let host = RemoteRef::new(target.clone(), WorkspaceId::new());
        let id = WorkspaceStore::claim_remote(cx, host);
        // The name is left unset on purpose: `Workspace::display_name` derives
        // it from the tabs' repo/cwd, which is the same rule a local workspace
        // follows and the one intended. A workspace that opened in
        // `~` and then had a repo opened in it renames itself for free.
        self.push_remote_layout(id, cx);
        log::info!(
            "new remote workspace on {target} rooted at {}",
            home.display()
        );
        self.enter_remote_workspace(id, window, cx);
    }

    /// Bind this window to a remote workspace.
    ///
    /// An empty window swaps in place — the connect flow only runs on the home
    /// page, so opening a second window would strand this blank one — and a
    /// window with tabs opens a new one, which is also what keeps the machines
    /// apart: the tabs already here stay with the workspace they belong to.
    fn enter_remote_workspace(
        &mut self,
        id: WorkspaceId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connect = None;
        // From here on the machine is the supervisor's business: it is what
        // notices the link dropping, retries on the backoff, and puts
        // the window read-only in between.
        RemoteLinks::ensure_running(cx);
        if self.tabs.is_empty() {
            let previous = self.spawn_host(cx);
            self.switch_workspace(id, window, cx);
            self.rebind_host(previous, cx);
        } else {
            crate::ui::windows::open(cx, Some(id));
        }
        cx.notify();
    }

    /// Push this window's workspace record to the machine that owns it.
    ///
    /// The other half of the storage split: the client keeps `open`, the window
    /// geometry and the pointer; everything that is a fact about the machine
    /// goes over there. Fire-and-forget on a background task — a failed push is
    /// a log line, not a modal, because the record is rewritten on every
    /// structural change anyway.
    pub(crate) fn push_remote_layout(&self, id: WorkspaceId, cx: &mut gpui::App) {
        let Some((host, key, record)) = WorkspaceStore::remote_payload(cx, id) else {
            return;
        };
        let Some(connection) = remote_connect::RemoteConnections::get(cx, host.host_id()) else {
            // Not connected: the layout is pushed again when it is, and the
            // remote's own copy is still the last good one.
            return;
        };
        // Marked before the task starts and cleared when it lands, so a
        // `WorkspaceChanged` that arrives in between does not pull the record
        // this push is replacing back over the top of it.
        cx.default_global::<RemoteLinks>().pushing.insert(id);
        cx.spawn(async move |cx| {
            cx.background_executor()
                .spawn(async move {
                    if let Err(e) = remote_connect::put_remote_layout(&connection, key, record) {
                        log::warn!(
                            "could not push the workspace layout to {}: {e}",
                            host.target
                        );
                    }
                })
                .await;
            cx.update(|cx| {
                cx.default_global::<RemoteLinks>().pushing.remove(&id);
            });
        })
        .detach();
    }

    /// `open: true` remote workspaces reconnect at launch.
    ///
    /// **M6 owns the behaviour**; this owns the seam. Startup opens a window per
    /// `open: true` workspace regardless of which machine it is on (`main.rs`
    /// never learned about hosts, which is exactly what we want — nothing on the
    /// launch path is hard-wired to local), and a remote one arrives here with
    /// its window already built and its layout the last one this client pulled.
    /// What M6 adds is the connect itself plus the auth queue that keeps ten
    /// windows from raising ten password sheets at once.
    pub(crate) fn reopen_remote_at_startup(&self, cx: &mut Context<Self>) {
        let Some(host) = WorkspaceStore::remote_ref(cx, self.workspace) else {
            return;
        };
        remote_connect::register(cx);
        if remote_connect::RemoteConnections::get(cx, host.host_id()).is_some() {
            // Another window on the same machine got there first. One connection
            // per machine is the point — D7's "connect immediately" is about the
            // *machine*, and a second link to it would be a second SSH session
            // for no reason.
            return;
        }
        log::info!("reconnecting to {} at startup", host.target);
        // No per-window connect call: the supervisor already knows which
        // machines have open workspaces, so starting it *is* the reconnect, and
        // ten windows on one box produce one attempt rather than ten.
        //
        // Nothing here classifies the host as needing authentication or not
        // (D7): every machine is attempted in parallel, and the ones that turn
        // out to need a human queue for the sheet at the moment they ask — see
        // [`AuthSheetQueue`].
        RemoteLinks::ensure_running(cx);
    }

    // ----- prompts -----------------------------------------------------------

    /// Install consent, relayed to the machine that raised it.
    ///
    /// A native prompt rather than an in-window sheet, matching every other
    /// decision in this class in tty7 ("Restart Daemon?", "Quit and Stop
    /// Daemon?"): it is modal by nature — a thread is parked on the answer — and
    /// the reason it exists is that it must not be dismissible by accident.
    ///
    /// **The default is to decline**, in three separate ways: the button order
    /// puts Cancel first, a dismissed prompt reads as Cancel, and an unanswered
    /// request times out into a decline back in `remote_connect`. Not being able
    /// to use a remote host because nobody said yes is the intended outcome.
    pub(crate) fn prompt_install_consent(
        &mut self,
        pending: remote_connect::PendingInstall,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = remote_connect::install_title(&pending.request);
        let detail = remote_connect::install_detail(&pending.request);
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(&detail),
            &["Cancel", "Install"],
            cx,
        );
        cx.spawn(async move |_, _| {
            // Index 1 is Install. Cancel, Escape and a prompt the user never
            // answered all mean the same thing.
            let decision = match answer.await {
                Ok(1) => InstallDecision::Approve,
                _ => InstallDecision::Decline,
            };
            pending.answer(decision);
        })
        .detach();
    }

    /// Version skew, for a remote server.
    ///
    /// Same shape as the local `prompt_daemon_version_mismatch`, and for the
    /// same reason: the running daemon owns every live pane on that machine, so
    /// this is a choice between an old dialect and losing the work. `take`
    /// semantics on the producer side mean this fires once per discovery rather
    /// than once per window.
    pub(crate) fn prompt_remote_daemon_mismatch(window: &mut Window, cx: &mut Context<Self>) {
        for mismatch in crate::daemon::install::take_mismatched_remote_daemons() {
            let title = remote_connect::mismatch_title(&mismatch);
            let detail = remote_connect::mismatch_detail(&mismatch);
            let answer = window.prompt(
                PromptLevel::Warning,
                &title,
                Some(&detail),
                &["Keep Sessions", "Restart Server"],
                cx,
            );
            cx.spawn(async move |this, cx| {
                // Index 1 is Restart Server. Dismissing the prompt keeps the
                // sessions, which is the answer that destroys nothing.
                if !matches!(answer.await, Ok(1)) {
                    return;
                }
                let _ = this.update_in(cx, |this, window, cx| {
                    this.restart_remote_server(mismatch, window, cx);
                });
            })
            .detach();
        }
    }

    /// Carry out "Restart Server": replace the `tty7-server` on a
    /// machine with this client's build.
    ///
    /// **This throws work away and says so.** Every pane the old server hosts
    /// dies with it — that is what the prompt the user just answered warns
    /// about, and it is why nothing on the connect path does this on its own.
    /// The supervisor reconnects the machine, finds a server whose instance id
    /// differs from the one it was talking to, and rebuilds each window from its
    /// layout: same tabs and splits, new shells, nothing running in them.
    fn restart_remote_server(
        &mut self,
        mismatch: crate::daemon::install::MismatchedRemoteDaemon,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = mismatch.host.clone();
        let header = match remote_connect::mismatch_target(&mismatch)
            .ok_or_else(|| format!("tty7 no longer has a way to reach {label}"))
            .and_then(|target| remote_connect::control_route(&target, cx))
        {
            Ok(header) => header.restart_server(),
            Err(e) => {
                Tty7App::report_restart_failure(&label, &e, window, cx);
                return;
            }
        };
        let host = header.target.origin_key();
        log::info!("restarting tty7's server on {label} at the user's request");
        // The same watcher the connect flow uses: a restart re-opens the
        // machine's connection, so it can raise a password sheet on the way in.
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        self.watch_for_restart_consent(running.clone(), cx);
        cx.spawn(async move |this, cx| {
            let for_task = label.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move { remote_connect::restart_server_blocking(header, &for_task) })
                .await;
            running.store(false, std::sync::atomic::Ordering::Relaxed);
            let _ = this.update_in(cx, |_, window, cx| match outcome {
                Ok(()) => {
                    log::info!("{label} is now serving this client's build");
                    reconnect_after_restart(&host, cx);
                }
                Err(e) => {
                    log::warn!("could not restart tty7's server on {label}: {e}");
                    Tty7App::report_restart_failure(&label, &e, window, cx);
                }
            });
        })
        .detach();
    }

    /// Tell the user a restart did not happen, rather than leaving the old
    /// server running behind a prompt that closed as if it had.
    ///
    /// The wording deliberately does **not** promise the sessions survived. Most
    /// failures here happen before anything is touched (no route, a local daemon
    /// too old, an unreachable machine), but one does not: the remote's old
    /// daemon is stopped before the new one is launched, so a failure to launch
    /// leaves a machine with no server and no sessions. Claiming "nothing was
    /// disturbed" would be a lie exactly when it matters most, and the recovery
    /// is the same either way — the supervisor reconnects, and reaching a
    /// machine with no server running starts one.
    fn report_restart_failure(
        label: &str,
        error: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("tty7's server on \u{201c}{label}\u{201d} was not restarted"),
            Some(&format!(
                "{error}\n\nSessions still running there are on the older build. If they are \
                 gone, reconnecting starts this build's server."
            )),
            &["OK"],
            cx,
        );
        cx.spawn(async move |_, _| {
            let _ = answer.await;
        })
        .detach();
    }

    /// Pump the routed prompt mailboxes while a restart is in flight.
    ///
    /// Same shape and same reason as [`Self::watch_for_install_consent`]: the
    /// restart blocks on the daemon, which may have to re-authenticate the
    /// machine first, and a question raised while that future is pending has
    /// nobody looking at its mailbox otherwise. The loop ends with the restart,
    /// so it costs nothing when nothing is restarting.
    fn watch_for_restart_consent(
        &self,
        running: Arc<std::sync::atomic::AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |_, cx| {
            while running.load(std::sync::atomic::Ordering::Relaxed) {
                cx.update(pump_auth_sheets);
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
        })
        .detach();
    }

    /// Raise the mismatch prompt on the next turn of the loop, from a context
    /// that has no `&mut Window` in hand.
    fn prompt_remote_daemon_mismatch_later(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let _ = this.update_in(cx, |_, window, cx| {
                Tty7App::prompt_remote_daemon_mismatch(window, cx);
            });
        })
        .detach();
    }
}

/// What a pane of `workspace` carries so its connection reaches the right
/// machine — the one function that turns a stored [`RemoteRef`] into the routing
/// input every pane-addressed call needs.
///
/// **`None` means local**, and every consumer treats it as "the path that
/// shipped": no route header, no extra frame, no behaviour to regress.
///
/// A remote workspace whose SSH details cannot be resolved still answers
/// `Some`, with no spec. That is deliberate: it becomes
/// [`PaneRoute::Unroutable`](crate::terminal::PaneRoute::Unroutable), which
/// *fails* rather than falling back to the local daemon. The fallback is the
/// dangerous answer — a `Kill { pane_id }` sent to the wrong daemon does not
/// error, it succeeds against a stranger's pane.
pub(crate) fn pane_workspace_for(
    cx: &gpui::App,
    workspace: WorkspaceId,
) -> Option<crate::terminal::PaneWorkspace> {
    let host = WorkspaceStore::remote_ref(cx, workspace)?;
    // Secret-free: the daemon matches the spec against a connection it has
    // already authenticated, so no credential needs to ride to it (design D3).
    let spec = remote_connect::spec_for(&host.target, cx)
        .ok()
        .map(|spec| Box::new(spec.without_secrets()));
    let pane = crate::terminal::PaneWorkspace {
        workspace,
        target: host.target,
        spec,
    };
    // The pane path's half of the routed-prompt attribution. A pane opens its
    // connection from `terminal::`, which cannot reach `ui::` and so cannot say
    // which machine a relayed password sheet belongs to; the header it will
    // write can, and this is where that header and this client's `HostId` are
    // both in hand. Taken from `route_header` itself rather than rebuilt, so the
    // key recorded is the key the pane actually sends.
    if let Ok(header) = pane.route_header() {
        remote_connect::note_origin(&header.target, &pane.target);
    }
    Some(pane)
}

/// Where a pane-addressed request about `workspace` has to go.
///
/// `Kill`, the restore-time `List` and the reconnect's `Attach` all name a
/// `pane_id`, and pane ids are **per daemon**. Sending one to the wrong daemon
/// is not a failed request — it is a successful request against somebody else's
/// pane.
pub(crate) fn pane_route_for(cx: &gpui::App, workspace: WorkspaceId) -> crate::terminal::PaneRoute {
    crate::terminal::PaneRoute::for_workspace(pane_workspace_for(cx, workspace).as_ref())
}

/// The connection for a remote workspace, if this process has one.
pub(crate) fn connection_for(
    cx: &mut gpui::App,
    workspace: WorkspaceId,
) -> Option<Arc<RemoteHost>> {
    let host = WorkspaceStore::remote_ref(cx, workspace)?;
    remote_connect::RemoteConnections::get(cx, host.host_id())
}

// ---------------------------------------------------------------------------
// The supervisor (the connection state machine, running)
// ---------------------------------------------------------------------------

/// How often the pump looks at every machine.
///
/// Fast enough that a `Preempted` push turns a window read-only while the user
/// is still looking at the machine they typed on, slow enough to be free: a tick
/// is a hash-map walk over the handful of machines a person has open.
const PUMP_TICK: Duration = Duration::from_millis(250);

/// One machine's link, as the supervisor sees it.
///
/// Per **machine**, not per workspace, because that is the granularity a
/// connection actually has (`RemoteConnections` is keyed by [`HostId`], and two
/// windows on one box share a link). Preemption is the one thing that is
/// per-workspace, and it is kept separately for exactly that reason.
struct MachineLink {
    state: LinkState,
    backoff: Backoff,
    /// When the next attempt is due. `None` while one is in flight or while the
    /// link is up.
    next_attempt: Option<Instant>,
    /// An attempt is running; the pump must not start a second one.
    attempting: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LinkState {
    Connecting,
    Attached,
    Reconnecting,
    Failed(String),
}

/// Every remote machine this client is trying to stay connected to.
///
/// Stored rather than derived — which is the change M5 flagged. "Is there a live
/// socket?" is derivable; "we lost it 4 seconds ago and will try again in 8" and
/// "somebody took this workspace" are *events*, and there is nowhere to read
/// them back from once they have happened.
#[derive(Default)]
pub(crate) struct RemoteLinks {
    machines: std::collections::HashMap<HostId, MachineLink>,
    /// Workspaces taken over, and by whom. Per **workspace**: one machine can
    /// hold three of them and lose exactly one.
    preempted: std::collections::HashMap<WorkspaceId, String>,
    /// Machines the user has deliberately disconnected from.
    ///
    /// Without this the supervisor would reconnect on the next tick: it keeps a
    /// link to every machine with an open workspace, and "the user asked us to
    /// stop" is not something it can read off the connection. Cleared by every
    /// path that asks to be connected again ([`RemoteLinks::retry_now`], the
    /// switcher's connect), and by [`pump_tick`] the moment a machine's last
    /// window closes — from there a reopened workspace connects like any other.
    suspended: std::collections::HashSet<HostId>,
    /// The `tty7-server` **process** each machine was last served by
    /// ([`crate::daemon::control::server_instance`]).
    ///
    /// Kept per machine and consulted on every reconnect, because it is the only
    /// thing that distinguishes the two ways a link comes back: to the same
    /// process (every `pane_id` still names the pane it always did) or to a new
    /// one (they name nothing, and the window has to be rebuilt). A machine
    /// absent from this map has never been seen before, which is **not** the
    /// same as having restarted — see [`finish_attempt`].
    instances: std::collections::HashMap<HostId, String>,
    /// Workspaces this client is pushing a layout for right now.
    ///
    /// Read by [`refresh_remote_workspace`], which skips them: a record we are
    /// in the middle of replacing is not one to pull back over the top of
    /// ourselves.
    pushing: std::collections::HashSet<WorkspaceId>,
    /// The start-up sheet queue. Lives here because it is part of the
    /// same connection state and has to survive individual windows — the sheet
    /// belongs to a machine, not to whichever window happened to ask first.
    #[allow(
        dead_code,
        reason = "read by the prompt relay's drain, which lands with the routed auth sheet"
    )]
    pub(crate) auth: AuthSheetQueue,
    /// Whether the single pump task is running.
    pumping: bool,
}

impl gpui::Global for RemoteLinks {}

/// Events the reader threads pushed, waiting for a turn on the UI thread.
///
/// A plain mutex rather than a channel because the producer is a reader thread
/// that must never block and the consumer is a 250 ms poll — the identical shape
/// `remote_connect`'s install mailbox uses, for the identical reason.
static EVENTS: Mutex<Vec<(HostId, ControlEvent)>> = Mutex::new(Vec::new());

impl RemoteLinks {
    /// Start the supervisor, and make sure control events have somewhere to go.
    ///
    /// Idempotent: called from every path that can produce a remote workspace
    /// (the connect flow, opening one, start-up), because any of them can be the
    /// first.
    pub(crate) fn ensure_running(cx: &mut gpui::App) {
        crate::daemon::control::set_event_observer(Arc::new(|host, event| {
            if let Ok(mut queue) = EVENTS.lock() {
                queue.push((host, event));
            }
        }));
        if cx.default_global::<RemoteLinks>().pumping {
            return;
        }
        cx.default_global::<RemoteLinks>().pumping = true;
        cx.spawn(async move |cx| {
            loop {
                let carry_on = cx.update(pump_tick);
                if !carry_on {
                    cx.update(|cx| cx.default_global::<RemoteLinks>().pumping = false);
                    return;
                }
                cx.background_executor().timer(PUMP_TICK).await;
            }
        })
        .detach();
    }

    /// This workspace's state, or `None` when it is a local one.
    pub(crate) fn status_of(cx: &gpui::App, workspace: WorkspaceId) -> Option<RemoteStatus> {
        let host = WorkspaceStore::remote_ref(cx, workspace)?;
        // No supervisor yet means no link — **not** "everything is fine". The
        // difference matters because this answer feeds the input gate, and
        // `None` there reads as "local, always accepts". A remote workspace
        // whose supervisor has not started is precisely a remote workspace that
        // is not connected, and typing into it must go nowhere.
        let Some(links) = cx.try_global::<RemoteLinks>() else {
            return Some(RemoteStatus::Disconnected);
        };
        // Preemption outranks the link's own state: the socket may well be fine
        // — the server closed it, or another of its workspaces is still live —
        // and "Attached" over a workspace somebody else is typing in would be a
        // lie with consequences.
        if let Some(by) = links.preempted.get(&workspace) {
            return Some(RemoteStatus::Preempted { by: by.clone() });
        }
        Some(match links.machines.get(&host.host_id()) {
            Some(link) => match &link.state {
                LinkState::Connecting => RemoteStatus::Connecting,
                LinkState::Attached => RemoteStatus::Attached,
                LinkState::Reconnecting => RemoteStatus::Reconnecting {
                    attempt: link.backoff.attempt(),
                },
                LinkState::Failed(e) => RemoteStatus::Failed(e.clone()),
            },
            None => RemoteStatus::Disconnected,
        })
    }

    /// The user asked to reconnect, or to take a preempted workspace back.
    ///
    /// Both are the same operation. The [抢回] is "接管一次" in the
    /// other direction, and a takeover *is* an attach — so clearing the
    /// preemption and letting the supervisor attach is not a shortcut, it is the
    /// mechanism.
    pub(crate) fn retry_now(cx: &mut gpui::App, workspace: WorkspaceId) {
        let Some(host) = WorkspaceStore::remote_ref(cx, workspace) else {
            return;
        };
        let links = cx.default_global::<RemoteLinks>();
        links.preempted.remove(&workspace);
        // Asking to reconnect outranks having asked to disconnect.
        links.suspended.remove(&host.host_id());
        let link = links.machines.entry(host.host_id()).or_insert(MachineLink {
            state: LinkState::Reconnecting,
            backoff: Backoff::default(),
            next_attempt: None,
            attempting: false,
        });
        // A deliberate act resets the schedule: the user pressing the button is
        // information the backoff does not have, and making them wait out a 30
        // second timer they just overrode would be absurd.
        link.backoff.reset();
        link.next_attempt = Some(Instant::now());
        if !link.attempting {
            link.state = LinkState::Reconnecting;
        }
        RemoteLinks::ensure_running(cx);
        cx.refresh_windows();
    }

    /// Stop holding a connection to `host`, because the user said so.
    ///
    /// **Nothing closes.** The windows on that machine stay exactly where they
    /// are and go read-only, which is the same resting state a dropped link
    /// leaves behind (a window is never closed automatically) — with the difference
    /// that this one is one click from being undone: no `MachineLink` at all
    /// reads as [`RemoteStatus::Disconnected`], and that state already draws a
    /// "Connect" button on the window's strip.
    ///
    /// Closing the user's windows for them would be a different, destructive
    /// act wearing the same word. If that is what they want, closing a window is
    /// already how it is said — and it disconnects too, by way of the machine
    /// going unbound.
    pub(crate) fn disconnect(cx: &mut gpui::App, host: HostId) {
        cx.default_global::<RemoteLinks>().suspended.insert(host);
        // Let go of the streams before the connection: a pane still holding one
        // would keep reading from a socket that is about to be dropped under it.
        for (workspace, _) in workspaces_on(cx, host) {
            release_panes(cx, workspace);
            cx.default_global::<RemoteLinks>()
                .preempted
                .remove(&workspace);
        }
        remote_connect::RemoteConnections::remove(cx, host);
        cx.default_global::<RemoteLinks>().machines.remove(&host);
        log::info!("disconnected from a machine at the user's request");
        cx.refresh_windows();
    }

    fn mark(cx: &mut gpui::App, host: HostId, f: impl FnOnce(&mut MachineLink)) {
        let link = cx
            .default_global::<RemoteLinks>()
            .machines
            .entry(host)
            .or_insert(MachineLink {
                state: LinkState::Connecting,
                backoff: Backoff::default(),
                next_attempt: None,
                attempting: false,
            });
        f(link);
    }
}

/// One turn of the supervisor. `false` ends the pump.
fn pump_tick(cx: &mut gpui::App) -> bool {
    drain_events(cx);
    // D7: start-up and reconnect are the two moments a dozen
    // machines can ask for a password at once, and both go through here.
    pump_auth_sheets(cx);

    let bound = bound_machines(cx);
    if bound.is_empty() {
        // Nothing left to supervise. The pump restarts the moment a remote
        // workspace opens again, so this costs nothing but a stopped timer.
        //
        // The preemption notes go too: they describe a session that no longer
        // exists, and a stale one would have a reopened workspace come back
        // read-only against a takeover that happened to a window that is gone.
        let links = cx.default_global::<RemoteLinks>();
        let forgotten = links.machines.len();
        links.machines.clear();
        links.preempted.clear();
        links.suspended.clear();
        // Logged because the *state* it leaves behind is indistinguishable from
        // never having connected: `status_of` reads a missing link as
        // `Disconnected`, so a window whose workspace is somehow not `open`
        // sits under a "Not connected" strip with a live machine behind it.
        // This line is how that is told apart from a real disconnect.
        log::info!("supervisor stopped: no open remote workspace ({forgotten} link(s) dropped)");
        return false;
    }

    prune_suspended(&mut cx.default_global::<RemoteLinks>().suspended, &bound);
    let suspended = cx.default_global::<RemoteLinks>().suspended.clone();

    let now = Instant::now();
    let mut changed = false;
    for (host, target) in bound {
        // Skipped before the liveness check, not after: the point is that this
        // machine gets no attempt at all, not that it gets a quieter one.
        if suspended.contains(&host) {
            continue;
        }
        let live = remote_connect::RemoteConnections::get(cx, host)
            .is_some_and(|h| h.client().is_connected());
        let attempting = cx
            .try_global::<RemoteLinks>()
            .and_then(|l| l.machines.get(&host))
            .is_some_and(|l| l.attempting);

        if live {
            let mut became = false;
            RemoteLinks::mark(cx, host, |link| {
                became = link.state != LinkState::Attached;
                link.state = LinkState::Attached;
                link.backoff.reset();
                link.next_attempt = None;
            });
            if became {
                changed = true;
                log::info!("link to {target} is attached");
            }
            continue;
        }
        if attempting {
            continue;
        }

        // The link is down. Drop the dead host object so nothing keeps calling
        // into it — a control connection that has gone is the whole
        // workspace's lifeline, not one failed request.
        if remote_connect::RemoteConnections::get(cx, host).is_some() {
            remote_connect::RemoteConnections::remove(cx, host);
            log::info!("lost the control connection to {target}; reconnecting");
        }

        let due = {
            let mut due = false;
            RemoteLinks::mark(cx, host, |link| {
                if !matches!(link.state, LinkState::Reconnecting) {
                    changed = true;
                    link.state = LinkState::Reconnecting;
                }
                match link.next_attempt {
                    None => link.next_attempt = Some(now + link.backoff.advance()),
                    Some(at) if at <= now => {
                        due = true;
                        link.next_attempt = None;
                    }
                    Some(_) => {}
                }
            });
            due
        };
        if due {
            launch_attempt(cx, host, target);
            changed = true;
        }
    }

    if changed {
        cx.refresh_windows();
    }
    true
}

/// A deliberate disconnect lasts exactly as long as there is a window to be
/// disconnected *in*.
///
/// Once a machine's last workspace closes, the state has nothing left to
/// describe — and remembering it would leave a workspace reopened an hour later
/// sitting offline for no reason the user could see, with no failure to point
/// at. Closing the window is itself an end to the connection; this is that,
/// written down.
///
/// Pure so the rule is a test rather than a comment.
fn prune_suspended(
    suspended: &mut std::collections::HashSet<HostId>,
    bound: &[(HostId, RemoteTarget)],
) {
    suspended.retain(|host| bound.iter().any(|(id, _)| id == host));
}

/// Every machine this client should be holding a connection to: the distinct
/// hosts of the remote workspaces whose windows are open.
///
/// Closed workspaces are excluded on purpose. A workspace with `open: false` is
/// one the user shut; reconnecting to it would hold an SSH connection open for a
/// window that is not there.
fn bound_machines(cx: &gpui::App) -> Vec<(HostId, RemoteTarget)> {
    let mut out: Vec<(HostId, RemoteTarget)> = Vec::new();
    for workspace in &WorkspaceStore::all(cx).workspaces {
        let Some(host) = workspace.host.as_ref() else {
            continue;
        };
        if !workspace.open {
            continue;
        }
        let id = host.host_id();
        if !out.iter().any(|(seen, _)| *seen == id) {
            out.push((id, host.target.clone()));
        }
    }
    out
}

/// The store keys of every open workspace on `host`, with the client ids they
/// belong to.
fn workspaces_on(cx: &gpui::App, host: HostId) -> Vec<(WorkspaceId, String)> {
    WorkspaceStore::all(cx)
        .workspaces
        .iter()
        .filter(|w| w.open)
        .filter_map(|w| {
            let remote = w.host.as_ref()?;
            (remote.host_id() == host).then(|| (w.id, remote.store_key()))
        })
        .collect()
}

/// Apply everything the reader threads pushed since the last tick.
fn drain_events(cx: &mut gpui::App) {
    let events = match EVENTS.lock() {
        Ok(mut queue) => std::mem::take(&mut *queue),
        Err(_) => return,
    };
    // Read out before the loop: the pull is one round trip per workspace no
    // matter how many events asked for it.
    let stale = stale_workspaces(&events);
    for (host, event) in events {
        match event {
            // The takeover, arriving. The window goes read-only and
            // **stays** — no automatic reconnect, because reconnecting is
            // taking it back, and taking it back is the user's decision.
            ControlEvent::Preempted { workspace, by } => {
                let Some(id) = client_id_for(cx, host, &workspace) else {
                    log::warn!(
                        "preempted on {host:?} for a workspace this client has no window on"
                    );
                    continue;
                };
                log::info!("workspace {id} was taken over by {by}");
                cx.default_global::<RemoteLinks>()
                    .preempted
                    .insert(id, by.clone());
                release_panes(cx, id);
                cx.refresh_windows();
            }
            // Handled by `stale_workspaces` above, in one pull per workspace.
            ControlEvent::WorkspaceChanged { .. } => {}
            other => log::debug!("unhandled control event from {host:?}: {other:?}"),
        }
    }
    for (host, key) in stale {
        refresh_remote_workspace(cx, host, key);
    }
}

/// The workspaces a batch of events says to re-read, each named once.
///
/// The machine's own record changed — another client of ours moved a tab,
/// renamed a workspace, closed one. The event carries **no record**, only "go
/// and read it again", and B3 is explicit that losing one of these is safe and
/// getting two is safe. That is exactly the licence to collapse a burst into one
/// round trip, and the reason nothing here tries to be incremental: there is no
/// state to keep, so there is none to get wrong.
fn stale_workspaces(events: &[(HostId, ControlEvent)]) -> Vec<(HostId, String)> {
    let mut out: Vec<(HostId, String)> = Vec::new();
    for (host, event) in events {
        if let ControlEvent::WorkspaceChanged { id } = event
            && !out.iter().any(|(h, key)| h == host && key == id)
        {
            out.push((*host, id.clone()));
        }
    }
    out
}

/// Re-read one workspace's record from the machine that owns it and apply it.
///
/// # Why this cannot interrupt what the user is doing
///
/// It lands in the **store**, not in the window. `apply_remote` writes the three
/// remote-owned fields of the client's `Workspace` entry (`name`, `session`,
/// `last_active`) and nothing rebuilds a live window from that entry while the
/// window is open — a workspace's tabs are built when the window opens or swaps
/// workspaces, and `claimable_session` scrubs a remote entry's layout even then.
/// So there is no path from here to a closed tab, a re-spawned pane or a moved
/// focus; what a user sees change is the workspace's *name*.
///
/// That is deliberate rather than incidental. The remote is the
/// authority for the layout, but the client that has the window open is the one
/// *living* in it, and rearranging somebody's panes underneath them because
/// another machine moved a tab is not a refresh, it is a fight. The remote's
/// layout is what a window opens *from* — on the next connect, reconnect or
/// reopen — and this keeps the copy it will open from current.
///
/// # The one race, and how it is settled
///
/// A push of ours can be in flight when an event arrives (the server excludes
/// the writer, so the event is another client's, but it may describe a moment
/// before our write). Applying it would briefly show that client's name for a
/// workspace we are mid-rename of. So a workspace with a push in flight is
/// skipped: our push is about to become the machine's truth, and B3's "dropping
/// one is safe" is what makes skipping the right move rather than a queue.
fn refresh_remote_workspace(cx: &mut gpui::App, host: HostId, store_key: String) {
    let Some(id) = client_id_for(cx, host, &store_key) else {
        // A workspace on that machine this client has no window on. Nothing to
        // refresh; the record is pulled when it is opened.
        log::debug!("remote workspace {store_key} changed on a machine with no window here");
        return;
    };
    if cx.default_global::<RemoteLinks>().pushing.contains(&id) {
        log::debug!("skipping the refresh of {id}: this client is mid-push for it");
        return;
    }
    let Some(connection) = remote_connect::RemoteConnections::get(cx, host) else {
        // Not connected: the next connect pulls the whole list anyway.
        return;
    };
    cx.spawn(async move |cx| {
        let pulled = cx
            .background_executor()
            .spawn(async move { remote_connect::get_remote_layout(&connection, store_key) })
            .await;
        match pulled {
            Ok(record) => {
                cx.update(|cx| {
                    WorkspaceStore::apply_remote(cx, id, &record);
                    cx.refresh_windows();
                });
            }
            // The workspace was deleted on the far side. The window stays open
            // with what it had — a window is never closed, and least of all
            // because another machine decided this one was done with it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("remote workspace {id} is gone from its machine; keeping the window");
            }
            Err(e) => log::warn!("could not re-read remote workspace {id}: {e}"),
        }
    })
    .detach();
}

/// Come back to a machine whose server was just replaced.
///
/// The connection this client held is to a process that no longer exists, so it
/// is dropped rather than waited out: the supervisor's own liveness check would
/// get there within a tick, but a dead host object in the registry is one the
/// file tree and the git status can still call into meanwhile.
///
/// `retry_now` (rather than letting the backoff schedule it) because a restart
/// the user asked for should come back at once — this is exactly the "the user
/// pressed the button is information the backoff does not have" case it exists
/// for.
///
/// **The sessions do not come back, but the window does.** Their pane ids named
/// panes in the old process, so nothing re-attaches; the reconnect notices the
/// new [`server_instance`](crate::daemon::control::server_instance) and rebuilds
/// each window from its layout — fresh shells in their saved cwds, agents
/// resumed where an id was captured. That is what "Restart Server ends every
/// session it is hosting" means: the work in them is gone, the window is not.
fn reconnect_after_restart(origin: &str, cx: &mut gpui::App) {
    let Some(host) = remote_connect::origin_host(origin) else {
        return;
    };
    remote_connect::RemoteConnections::remove(cx, host);
    for (workspace, _) in workspaces_on(cx, host) {
        RemoteLinks::retry_now(cx, workspace);
    }
    RemoteLinks::ensure_running(cx);
    cx.refresh_windows();
}

/// The client's id for the workspace the remote calls `store_key` on `host`.
fn client_id_for(cx: &gpui::App, host: HostId, store_key: &str) -> Option<WorkspaceId> {
    workspaces_on(cx, host)
        .into_iter()
        .find(|(_, key)| key == store_key)
        .map(|(id, _)| id)
}

/// One reconnect attempt, off the UI thread.
///
/// The sequence, in order: rebuild the control connection, pull the
/// workspace layout, re-attach each workspace. The pane half — reopen a channel
/// per pane, `Attach`, replay, then `Resize` to *this* client's geometry — hangs
/// off [`relink_panes`], which is where it lands when remote panes exist.
fn launch_attempt(cx: &mut gpui::App, host: HostId, target: RemoteTarget) {
    let label = target.to_string();
    let header = match remote_connect::control_route(&target, cx) {
        Ok(header) => header,
        Err(e) => {
            // A profile that has been deleted is not a network failure, and
            // retrying it for ever would be. This is a resting state with a
            // button.
            RemoteLinks::mark(cx, host, |link| {
                link.state = LinkState::Failed(e);
                link.next_attempt = None;
                link.attempting = false;
            });
            cx.refresh_windows();
            return;
        }
    };
    // Resolved before the task starts: the workspace list is UI-thread state,
    // and the background half must not reach back for it.
    let keys: Vec<String> = workspaces_on(cx, host)
        .into_iter()
        .map(|(_, key)| key)
        .collect();

    RemoteLinks::mark(cx, host, |link| link.attempting = true);
    cx.spawn(async move |cx| {
        let label_for_task = label.clone();
        let outcome = cx
            .background_executor()
            .spawn(async move {
                let connected = remote_connect::connect_blocking(&target, header, &label_for_task)?;
                // The attach is what makes this client the
                // workspace's session again — and what preempts whoever took it
                // while we were away.
                for key in &keys {
                    match connected
                        .host
                        .client()
                        .call(ControlRequest::WorkspaceAttach { id: key.clone() })
                    {
                        Ok(ReplyOk::Attached {
                            took_over_from: Some(who),
                        }) => {
                            log::info!("took workspace {key} back from {who}");
                        }
                        Ok(_) => {}
                        // A machine that has no workspace store (an older
                        // server) still serves files; the workspace is usable,
                        // it simply cannot be claimed exclusively.
                        Err(e) => log::warn!("could not attach to workspace {key}: {e}"),
                    }
                }
                Ok::<_, String>(connected)
            })
            .await;
        cx.update(|cx| finish_attempt(cx, host, &label, outcome));
    })
    .detach();
}

/// Land a finished reconnect attempt.
fn finish_attempt(
    cx: &mut gpui::App,
    host: HostId,
    label: &str,
    outcome: Result<remote_connect::Connected, String>,
) {
    match outcome {
        Ok(connected) => {
            // The remote's record is the authority for the layout,
            // so what came back with the connect replaces what this client had.
            let rows = connected.rows.clone();
            let instance = connected.host.peer().instance.clone();
            let restarted = server_restarted(cx, host, &connected.host);
            // The home too, not just the connection: this is the path a machine
            // comes back on after a restart or a dropped link, and dropping it
            // here is what left "New Workspace" missing on a machine the panel
            // was quite happily calling connected.
            remote_connect::RemoteConnections::insert(cx, connected.host, connected.home);
            for (id, key) in workspaces_on(cx, host) {
                if let Some(row) = rows.iter().find(|r| r.id.to_string() == key) {
                    WorkspaceStore::apply_remote(cx, id, &row.record);
                }
                cx.default_global::<RemoteLinks>().preempted.remove(&id);
                // The same question `restarted` answers, asked of the *record*
                // rather than of this process's memory — and it is the only one
                // that can answer across a client restart. `instances` is an
                // in-memory map, so on a cold launch every machine is a first
                // sighting and `restarted` is false; a server replaced while
                // this client was closed would sail through, and its recycled
                // ids would attach to whatever unrelated shells now hold the
                // numbers. `daemon_instance` is on disk and remembers.
                let stale = WorkspaceStore::forget_stale_pane_ids(cx, id, &instance);
                if restarted || stale {
                    // Every pane id this workspace holds was minted by a process
                    // that is gone. Re-attaching them would cost one doomed round
                    // trip each and leave the window exactly as disconnected as
                    // it is now, so the window is rebuilt from the layout instead
                    // — the same thing a local daemon restart does.
                    rebuild_after_server_restart(cx, id);
                } else {
                    relink_panes(cx, id);
                    // A window that came up before its machine did has no panes to
                    // relink — it opened empty because there was nothing to route
                    // to. Now there is.
                    hydrate_window(cx, id);
                }
                // Same reason the window had no panes: with the machine
                // unreachable there was nothing to ask for its shells, so the
                // "+" dropdown has been sitting on the empty fallback.
                refresh_window_shells(cx, id);
            }
            RemoteLinks::mark(cx, host, |link| {
                link.state = LinkState::Attached;
                link.backoff.reset();
                link.next_attempt = None;
                link.attempting = false;
            });
            log::info!("reconnected to {label}");
        }
        Err(e) => {
            log::warn!("reconnect to {label} failed: {e}");
            RemoteLinks::mark(cx, host, |link| {
                link.attempting = false;
                link.state = LinkState::Reconnecting;
                // The next tick schedules the following attempt off the
                // advanced backoff; setting it here would double-count.
                link.next_attempt = None;
            });
        }
    }
    cx.refresh_windows();
}

/// The pane half of a reconnect: for each pane, reopen a channel,
/// `Attach`, take the replay, then `Resize` to this client's geometry.
///
/// # The replay boundary
///
/// `ReplayRing` is 8 MiB over at most 64 segments. A pane disconnected long
/// enough to overrun it comes back with the daemon's current grid snapshot and
/// **the middle is genuinely gone**. That is the same thing a local pane does
/// after a daemon restart, it is not fixable from this side, and nothing here
/// papers over it: no "restoring…" spinner that implies the gap will fill in, no
/// synthesized scrollback, and no attempt to stitch the pre-drop screen onto the
/// replay — [`RemoteTerminal::adopt_relink`](crate::terminal::RemoteTerminal::adopt_relink)
/// resets the mirror precisely so what is on screen afterwards is what the
/// machine actually has.
///
/// # Why each pane goes through a background task
///
/// The re-`Attach` is a network round trip on a link that has just been rebuilt,
/// and a `TerminalView` can only be touched on the UI thread. So the blocking
/// half runs off it and only the socket swap comes back — the same split every
/// other connect in this module makes.
fn relink_panes(cx: &mut gpui::App, workspace: WorkspaceId) {
    let route = pane_route_for(cx, workspace);
    if matches!(route, crate::terminal::PaneRoute::Local) {
        // Not a remote workspace: there is no route to redo, and a local pane
        // must never be re-attached by this path.
        return;
    }
    let panes = panes_of(cx, workspace);
    if panes.is_empty() {
        return;
    }
    log::info!("relinking {} pane(s) of workspace {workspace}", panes.len());
    for view in panes {
        let (pane_id, size, cell_w, cell_h) = view.read(cx).relink_plan();
        let opening = route.clone();
        let adopting = route.clone();
        cx.spawn(async move |cx| {
            let opened = cx
                .background_executor()
                .spawn(async move {
                    crate::terminal::RemoteTerminal::open_relink(
                        &opening, pane_id, size, cell_w, cell_h,
                    )
                    .map_err(|e| e.to_string())
                })
                .await;
            match opened {
                Ok(stream) => {
                    view.update(cx, |view, cx| {
                        if let Err(e) =
                            view.adopt_relink(stream, &adopting, size, cell_w, cell_h, cx)
                        {
                            log::warn!("pane {pane_id} re-attached but could not be adopted: {e}");
                        }
                    });
                }
                // A pane that could not come back stays on screen in its
                // disconnected state. The supervisor is still retrying the
                // machine, and the next success runs this again.
                Err(e) => log::warn!("could not relink pane {pane_id}: {e}"),
            }
        })
        .detach();
    }
}

/// Build the tabs of a window that opened before its machine was reachable.
///
/// This is the other end of [`crate::core::session::WorkspaceStore::claim`]'s
/// reachability rule. A remote workspace reopened at launch has nowhere to route
/// to yet — the link is still being built — so it opens empty rather than
/// spawning a second set of shells beside the ones still running over there.
/// The layout it *would* have opened from is the entry's cached session, which
/// [`finish_attempt`] has just refreshed from the machine itself, so by the time
/// this runs the window is rebuilding from the authority.
///
/// # What it will not do
///
/// **Only an empty window is touched.** A window with tabs is one the user is
/// working in; rearranging it because a link came back is the same fight
/// [`refresh_remote_workspace`] refuses to pick. That also makes this safe to
/// call on every reconnect — the second one through finds tabs and leaves.
fn hydrate_window(cx: &mut gpui::App, workspace: WorkspaceId) {
    let session = match WorkspaceStore::all(cx).get(workspace) {
        Some(entry) if entry.is_remote() => entry.session.clone(),
        // Local, or an entry that went away while the connect was in flight.
        _ => return,
    };
    if session.tabs.is_empty() {
        // Nothing to restore: a workspace that was quit from the home page, or
        // a brand-new one. Its window is right as it is.
        return;
    }
    let Some(handle) = crate::ui::windows::WindowRegistry::window_for(cx, workspace) else {
        return;
    };
    let Some(app) =
        crate::ui::windows::WindowRegistry::app_for(cx, workspace).and_then(|app| app.upgrade())
    else {
        return;
    };
    if !app.read(cx).tabs.is_empty() {
        return;
    }
    log::info!(
        "rebuilding {} tab(s) of workspace {workspace} now its machine is reachable",
        session.tabs.len()
    );
    let _ = handle.update(cx, move |_, window, cx| {
        app.update(cx, |app, cx| {
            app.adopt_workspace(workspace, session, window, cx)
        });
    });
}

/// Whether the machine we just reconnected to is being served by a *different*
/// `tty7-server` process than the one we last spoke to.
///
/// `true` is a statement of fact, not a guess, and that is the whole point: it
/// is the difference between a link that blinked (re-attach; the shells are
/// still running over there) and a server that was replaced (rebuild; they are
/// not). Before the instance id existed there was nothing to tell them apart —
/// `build` and both dialect numbers survive a restart unchanged — so the
/// reconnect had to assume the safer of the two and leave dead panes on screen.
///
/// Two cases answer `false` and mean different things, both deliberately:
///
/// | | |
/// |---|---|
/// | No previous instance recorded | First time we have reached this machine. Nothing was attached, so nothing was lost |
/// | The peer reported no instance | We cannot tell. Never treat "don't know" as "restarted" — that would throw away live shells on a hunch |
fn server_restarted(cx: &mut gpui::App, host: HostId, peer: &RemoteHost) -> bool {
    let instance = peer.peer().instance.clone();
    let seen = &mut cx.default_global::<RemoteLinks>().instances;
    note_instance(seen, host, &instance)
}

/// Record `instance` as what is serving `host` and answer whether it displaced a
/// *different* one. Split out from [`server_restarted`] because the rule matters
/// more than the plumbing around it and is worth a test that doesn't need a
/// connection to run.
fn note_instance(
    seen: &mut std::collections::HashMap<HostId, String>,
    host: HostId,
    instance: &str,
) -> bool {
    if instance.is_empty() {
        // Nothing learned, so nothing recorded: writing an empty value would
        // make the *next* reconnect compare against it and read a real instance
        // as a restart.
        return false;
    }
    match seen.insert(host, instance.to_string()) {
        Some(before) if before != instance => {
            log::info!(
                "the tty7-server on this machine is a new process ({before} → {instance}); \
                 its panes are gone"
            );
            true
        }
        _ => false,
    }
}

/// Rebuild a workspace's window after its machine's server was replaced.
///
/// The local analogue is [`Tty7App::restart_daemon_confirmed`], and this is
/// deliberately the same shape: the layout is the thing that survives, and every
/// leaf in it comes back as a fresh shell in its saved cwd. What makes it safe
/// here is only that [`server_restarted`] *knew* — the same rebuild triggered by
/// a guess would be a way to lose running work.
///
/// **The saved pane ids are dropped first**, and that is what makes the resume
/// work rather than being a tidiness measure. `session_to_pane` keeps a remote
/// leaf's id unconditionally (its liveness cannot be probed without a round
/// trip) and lets the attach fail into a spawn *inside* the terminal — by which
/// point the code that would have sent `claude --resume <id>` has already
/// decided it wasn't needed. Clearing the ids up here makes the leaf take the
/// same path a dead local pane takes, so the agent conversation continues.
///
/// Unlike [`hydrate_window`] this does **not** skip a window with tabs. Those
/// tabs are precisely what has to go: every one of them is a pane bound to a
/// process that no longer exists.
fn rebuild_after_server_restart(cx: &mut gpui::App, workspace: WorkspaceId) {
    let mut session = match WorkspaceStore::all(cx).get(workspace) {
        Some(entry) if entry.is_remote() => entry.session.clone(),
        _ => return,
    };
    if session.tabs.is_empty() {
        return;
    }
    for tab in &mut session.tabs {
        tty7_core::core::session::blank_pane_ids(&mut tab.pane);
    }
    let Some(handle) = crate::ui::windows::WindowRegistry::window_for(cx, workspace) else {
        return;
    };
    let Some(app) =
        crate::ui::windows::WindowRegistry::app_for(cx, workspace).and_then(|app| app.upgrade())
    else {
        return;
    };
    log::info!(
        "rebuilding {} tab(s) of workspace {workspace}: its machine is serving a new process",
        session.tabs.len()
    );
    let _ = handle.update(cx, move |_, window, cx| {
        app.update(cx, |app, cx| {
            app.adopt_workspace(workspace, session, window, cx)
        });
    });
}

/// Ask the window showing `workspace` to refill its "+" dropdown, now that its
/// machine is answering. No-op for a workspace with no window on screen.
fn refresh_window_shells(cx: &mut gpui::App, workspace: WorkspaceId) {
    let Some(app) =
        crate::ui::windows::WindowRegistry::app_for(cx, workspace).and_then(|app| app.upgrade())
    else {
        return;
    };
    app.update(cx, |app, cx| app.refresh_shells(cx));
}

/// The takeover, on this client's side: stop holding a stream to a
/// workspace somebody else is now typing in.
///
/// The remote closes them too — this is not the mechanism, it is the client
/// making sure. The panes stay on screen (read-only, never auto-closed);
/// only the links go.
fn release_panes(cx: &mut gpui::App, workspace: WorkspaceId) {
    for view in panes_of(cx, workspace) {
        view.update(cx, |view, cx| view.detach_link(cx));
    }
}

// ---------------------------------------------------------------------------
// The start-up auth queue, wired (D7)
// ---------------------------------------------------------------------------

/// Move every routed auth prompt one step: drain the mailbox, offer each to the
/// queue, and raise the sheet for whichever machine's turn it is.
///
/// The rule is "**一次只弹一个 sheet，其余排队**", and the queue is keyed
/// by machine because the credential is the machine's — ten windows restoring
/// onto one box must ask for one password, not ten.
///
/// A prompt that does not get the sheet is **parked, not dropped**. A dropped
/// [`PendingAuth`](remote_connect::PendingAuth) leaves a connect thread blocked
/// until the 180s consent timeout, which is exactly the "the app hung on launch"
/// this queue exists to prevent.
///
/// Called from two places, because there are two moments a routed prompt can
/// arrive and neither covers the other: the connect watcher (a picker connect,
/// when no workspace is bound to the machine yet) and the supervisor's tick
/// (start-up and every reconnect, when no picker is open).
pub(crate) fn pump_auth_sheets(cx: &mut gpui::App) {
    let mut inbox: Vec<remote_connect::PendingAuth> = Vec::new();
    while let Some(pending) = remote_connect::take_pending_auth() {
        inbox.push(pending);
    }
    // Anything that waited its turn last time goes back in the same pass, so a
    // sheet that has since closed hands straight on rather than waiting for the
    // next machine to ask something.
    if let Ok(mut parked) = PARKED.lock() {
        inbox.append(&mut parked);
    }

    for pending in inbox {
        let host = pending.host;
        if !cx.default_global::<RemoteLinks>().auth.request(host) {
            park(pending);
            continue;
        }
        match raise_auth_sheet(cx, pending) {
            SheetOutcome::Raised => {}
            // Nobody could show it. Give the turn back so the next machine is
            // not stuck behind a sheet that never appeared.
            outcome => {
                cx.default_global::<RemoteLinks>().auth.release(host);
                if let SheetOutcome::GiveBack(pending) = outcome {
                    park(pending);
                }
            }
        }
    }
}

/// What became of an attempt to show one machine's sheet.
///
/// An enum rather than a `Result` because "nobody could show it" is not an
/// error — it is the queue working — and because the prompt has to travel back
/// intact either way: dropping a [`PendingAuth`](remote_connect::PendingAuth)
/// leaves a connect thread parked until its 180s timeout.
pub(crate) enum SheetOutcome {
    /// It is on screen, or was a banner and is already answered. The queue is
    /// released when the sheet resolves.
    Raised,
    /// No window on that machine yet, or another sheet is up. Park it and offer
    /// it again on the next pump.
    GiveBack(remote_connect::PendingAuth),
    /// It went down with a window that closed mid-flight. The connect times out
    /// and cancels — the same answer it would have got with no UI at all.
    Lost,
}

/// Put the sheet in front of a person, on the window of a workspace on that
/// machine.
fn raise_auth_sheet(cx: &mut gpui::App, pending: remote_connect::PendingAuth) -> SheetOutcome {
    let host = pending.host;
    let Some((workspace, _)) = workspaces_on(cx, host).into_iter().next() else {
        return SheetOutcome::GiveBack(pending);
    };
    let Some(handle) = crate::ui::windows::WindowRegistry::window_for(cx, workspace) else {
        return SheetOutcome::GiveBack(pending);
    };
    let Some(app) =
        crate::ui::windows::WindowRegistry::app_for(cx, workspace).and_then(|app| app.upgrade())
    else {
        return SheetOutcome::GiveBack(pending);
    };
    handle
        .update(cx, move |_, window, cx| {
            app.update(cx, |app, cx| app.raise_routed_auth(pending, window, cx))
        })
        .unwrap_or(SheetOutcome::Lost)
}

/// Hand the sheet on when one machine's question is done with.
///
/// Called from the sheet itself (`ui::ssh_prompt`), on every way out of it —
/// answered, cancelled or dismissed. Parked prompts are re-offered by the next
/// [`pump_auth_sheets`], so nothing here has to know who goes next.
pub(crate) fn release_auth_sheet(host: HostId, cx: &mut gpui::App) {
    cx.default_global::<RemoteLinks>().auth.release(host);
}

/// Prompts waiting for a turn.
///
/// A process-wide mutex like the mailbox they came from, because a prompt
/// outlives the window that happened to poll for it: a machine can lose its
/// window while its connect thread is still parked on the answer.
static PARKED: Mutex<Vec<remote_connect::PendingAuth>> = Mutex::new(Vec::new());

fn park(pending: remote_connect::PendingAuth) {
    if let Ok(mut parked) = PARKED.lock() {
        parked.push(pending);
    }
}

/// Every pane of the window showing `workspace`, or empty when no window does.
fn panes_of(
    cx: &mut gpui::App,
    workspace: WorkspaceId,
) -> Vec<gpui::Entity<crate::terminal::view::TerminalView>> {
    let Some(app) = crate::ui::windows::WindowRegistry::app_for(cx, workspace) else {
        return Vec::new();
    };
    let Some(app) = app.upgrade() else {
        return Vec::new();
    };
    app.read(cx).panes()
}

/// Whether keystrokes should reach the panes of `workspace`.
///
/// **The one call the pane layer makes.** The read-only degrade has to
/// be checked at every point a keystroke can enter a pane — `on_key_down`, the
/// IME's `commit_text`, `paste`, `send_to_pty`, and the typeahead `dump_hold`
/// timer — and a rule copied five times is a rule that will disagree with itself
/// after the first change here.
///
/// A local workspace always answers `true`: it has no connection to lose, and a
/// gate that could ever say otherwise for a local pane would be a bug that
/// bricks the app.
///
/// **Nothing is buffered** (D6). This is a gate, and the deliberate absence of a
/// queue behind it is the decision: keystrokes typed while disconnected are
/// dropped, because replaying them into a screen that has moved on is how a
/// stray `y` becomes an answer to a question the user never read.
#[allow(
    dead_code,
    reason = "the five keystroke entry points that call this live in terminal/view.rs"
)]
pub(crate) fn workspace_accepts_input(cx: &gpui::App, workspace: WorkspaceId) -> bool {
    RemoteLinks::status_of(cx, workspace).is_none_or(|s| s.accepts_input())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_strip_speaks_unless_everything_is_working() {
        assert_eq!(RemoteStatus::Attached.strip_message("build-box"), None);
        // Each state says its own thing once — an earlier cut phrased the
        // no-connection case through `Failed("not connected")` and the strip
        // read "Not connected to build-box — not connected".
        assert_eq!(
            RemoteStatus::Disconnected
                .strip_message("build-box")
                .as_deref(),
            Some("Not connected to build-box")
        );
        assert_eq!(
            RemoteStatus::Connecting
                .strip_message("build-box")
                .as_deref(),
            Some("Connecting to build-box…")
        );
        assert_eq!(
            RemoteStatus::Failed("connection refused".into())
                .strip_message("build-box")
                .as_deref(),
            Some("Not connected to build-box — connection refused")
        );
    }

    /// The read-only degrade: a window that is connecting or failed
    /// still shows and scrolls, but typing goes nowhere — and is not buffered
    /// for later (D6), which is why this is a gate and not a queue.
    #[test]
    fn input_is_gated_on_being_attached() {
        assert!(RemoteStatus::Attached.accepts_input());
        assert!(!RemoteStatus::Disconnected.accepts_input());
        assert!(!RemoteStatus::Connecting.accepts_input());
        assert!(!RemoteStatus::Failed("x".into()).accepts_input());
    }

    /// Both remaining states name their machine — which is what lets the
    /// switcher give a machine a group of its own while it is still being
    /// reached, or after it failed. Picking one out of a list is no longer a
    /// state of the flow at all; `ui::switcher` is that list.
    #[test]
    fn every_flow_state_names_its_machine() {
        let choice = HostChoice {
            target: RemoteTarget::Alias {
                alias: "build-box".into(),
            },
            label: "build-box".into(),
            detail: "me@build-box".into(),
        };
        let flow = ConnectFlow::Connecting {
            choice: choice.clone(),
        };
        assert_eq!(flow.choice(), Some(&choice));
        let flow = ConnectFlow::Failed {
            choice: choice.clone(),
            error: "no route to host".into(),
        };
        assert_eq!(flow.choice(), Some(&choice));
    }

    /// The rule that decides re-attach vs rebuild. Getting any of these four
    /// wrong costs the user running work: a false positive rebuilds a window
    /// whose shells were fine, a false negative leaves a screen of dead panes.
    #[test]
    fn only_a_changed_instance_counts_as_a_restart() {
        let mut seen = std::collections::HashMap::new();
        let host = HostId::from_connection_key("ssh:build-box");

        // First sight of a machine. Nothing was attached to the old process
        // because there was no old process *we* knew about.
        assert!(!note_instance(&mut seen, host, "abc"));
        // The link blinked and came back to the same server.
        assert!(!note_instance(&mut seen, host, "abc"));
        // Replaced.
        assert!(note_instance(&mut seen, host, "def"));
        // …and the new one is now the baseline, so it is not a restart twice.
        assert!(!note_instance(&mut seen, host, "def"));
    }

    /// A peer that reports no instance leaves no trace. Recording the empty
    /// string would make the *next* reconnect — one that does report an id —
    /// read as a restart and throw away live shells.
    #[test]
    fn an_unknown_instance_is_not_a_restart_and_is_not_remembered() {
        let mut seen = std::collections::HashMap::new();
        let host = HostId::from_connection_key("ssh:build-box");

        assert!(!note_instance(&mut seen, host, ""));
        assert!(seen.is_empty(), "an unknown instance must not be recorded");
        assert!(
            !note_instance(&mut seen, host, "abc"),
            "the first real instance is a first sighting, not a restart"
        );
    }

    /// Two machines are tracked apart. They mint instance ids independently, so
    /// one restarting must not rebuild the other's windows.
    #[test]
    fn instances_are_per_machine() {
        let mut seen = std::collections::HashMap::new();
        let a = HostId::from_connection_key("ssh:box-a");
        let b = HostId::from_connection_key("ssh:box-b");

        assert!(!note_instance(&mut seen, a, "a1"));
        assert!(!note_instance(&mut seen, b, "b1"));
        assert!(note_instance(&mut seen, a, "a2"));
        assert!(
            !note_instance(&mut seen, b, "b1"),
            "box-b never changed; box-a restarting is not its business"
        );
    }

    /// Every leaf loses its id, at every depth. A `Split` branch that kept its
    /// ids would leave those panes attaching to a dead process — and, worse,
    /// skipping the agent resume, because that only fires for a leaf with no id.
    #[test]
    fn forgetting_pane_ids_reaches_every_leaf() {
        use crate::core::session::{SessionAxis, SessionPane};

        fn leaf(id: u64) -> SessionPane {
            SessionPane::Leaf {
                cwd: None,
                pane_id: Some(id),
                ssh_spec: None,
                agent: None,
                agent_session_id: None,
                agent_launch_argv: None,
            }
        }
        fn ids(pane: &SessionPane, out: &mut Vec<Option<u64>>) {
            match pane {
                SessionPane::Leaf { pane_id, .. } => out.push(*pane_id),
                SessionPane::Split { a, b, .. } => {
                    ids(a, out);
                    ids(b, out);
                }
            }
        }

        let mut pane = SessionPane::Split {
            axis: SessionAxis::Horizontal,
            ratio: 0.5,
            a: Box::new(leaf(1)),
            b: Box::new(SessionPane::Split {
                axis: SessionAxis::Vertical,
                ratio: 0.5,
                a: Box::new(leaf(2)),
                b: Box::new(leaf(3)),
            }),
        };
        let forgotten = tty7_core::core::session::blank_pane_ids(&mut pane);

        let mut found = Vec::new();
        ids(&pane, &mut found);
        assert_eq!(found, vec![None, None, None]);
        assert_eq!(forgotten, 3, "every dropped claim is counted");
    }

    // ── The reconnect schedule (no network) ─────────────────────────────────

    /// The schedule is fixed: **1/2/4/…/30s capped, retried for ever**.
    #[test]
    fn the_backoff_doubles_to_thirty_seconds_and_stays_there() {
        let mut b = Backoff::default();
        let seen: Vec<u64> = (0..8).map(|_| b.advance().as_secs()).collect();
        assert_eq!(seen, vec![1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(b.attempt(), 8, "every attempt is counted, capped or not");
    }

    /// A connection that succeeds puts the next failure back at one second —
    /// otherwise a link that flaps once an hour ends up on the 30s ceiling for
    /// ever and feels broken.
    #[test]
    fn a_success_resets_the_schedule() {
        let mut b = Backoff::default();
        for _ in 0..5 {
            b.advance();
        }
        assert_eq!(b.delay(), RECONNECT_CAP);
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_eq!(b.delay(), RECONNECT_FIRST);
    }

    /// It never gives up and it never panics. A supervisor left running for a
    /// week reaches exponents where `1 << n` is undefined behaviour in C and a
    /// debug panic in Rust; the cap has to absorb every one of them.
    #[test]
    fn the_backoff_survives_absurd_attempt_counts() {
        let mut b = Backoff::default();
        for _ in 0..200 {
            assert!(b.advance() <= RECONNECT_CAP);
        }
        assert_eq!(b.delay(), RECONNECT_CAP, "still retrying, still capped");
    }

    // ── The start-up auth queue (D7) ─────────────────────────────

    fn host(key: &str) -> HostId {
        HostId::from_connection_key(key)
    }

    /// D7: **one sheet at a time, the rest queue.** Ten windows restoring at
    /// once must not raise ten password prompts.
    #[test]
    fn only_one_machine_may_raise_a_sheet_at_a_time() {
        let mut q = AuthSheetQueue::default();
        let (a, b, c) = (
            host("ssh-alias:a"),
            host("ssh-alias:b"),
            host("ssh-alias:c"),
        );

        assert!(q.request(a), "the first asker goes straight through");
        assert!(!q.request(b));
        assert!(!q.request(c));
        assert_eq!(q.waiting(), 2);
        assert_eq!(q.holder(), Some(a));

        // Answering hands the sheet to the next in line, in order.
        assert_eq!(q.release(a), Some(b));
        assert_eq!(q.waiting(), 1);
        assert_eq!(q.release(b), Some(c));
        assert_eq!(q.release(c), None);
        assert_eq!(q.holder(), None);
        assert_eq!(q.waiting(), 0);
    }

    /// One connect asks twice — a key passphrase, then an unknown host key. The
    /// second ask must not queue behind the sheet it is already holding.
    #[test]
    fn the_holder_may_ask_again_without_deadlocking() {
        let mut q = AuthSheetQueue::default();
        let a = host("ssh-alias:a");
        assert!(q.request(a));
        assert!(q.request(a));
        assert_eq!(q.waiting(), 0);
    }

    /// A queued connect that dies on its own (the machine is unreachable) has to
    /// leave the queue, or the sheet is handed to a host nobody is waiting on
    /// and the next one waits behind a ghost.
    #[test]
    fn a_connect_that_gave_up_leaves_the_queue() {
        let mut q = AuthSheetQueue::default();
        let (a, b, c) = (
            host("ssh-alias:a"),
            host("ssh-alias:b"),
            host("ssh-alias:c"),
        );
        q.request(a);
        q.request(b);
        q.request(c);

        q.withdraw(b);
        assert_eq!(q.waiting(), 1);
        assert_eq!(q.release(a), Some(c), "b is gone, c is next");

        // And a release from someone who never held it changes nothing.
        assert_eq!(q.release(b), None);
        assert_eq!(q.holder(), Some(c));
    }

    /// The queue is keyed by *machine*, not by window: two windows on one box
    /// share a connection and a credential, and asking twice for one password is
    /// the behaviour this exists to prevent.
    #[test]
    fn two_windows_on_one_machine_share_a_place_in_the_queue() {
        let mut q = AuthSheetQueue::default();
        let (a, b) = (host("ssh-alias:build"), host("ssh-alias:other"));
        assert!(q.request(a));
        assert!(!q.request(b));
        // The same machine asking again from its second window is already the
        // holder, not a third entry.
        assert!(q.request(a));
        assert_eq!(q.waiting(), 1);
    }

    // ── `WorkspaceChanged` → re-read (B3's push, arriving) ───────────────────

    /// **A burst of changes costs one round trip per workspace.**
    ///
    /// B3's contract for this event is that it means only "read it again", so
    /// losing one is safe and getting ten is safe. Collapsing them is the whole
    /// of the logic that rule buys — and the thing that keeps a client with a
    /// chatty peer from opening a `WorkspaceGet` per keystroke of theirs.
    #[test]
    fn a_burst_of_changes_asks_for_each_workspace_once() {
        let (a, b) = (host("ssh-alias:a"), host("ssh-alias:b"));
        let changed = |id: &str| ControlEvent::WorkspaceChanged { id: id.into() };
        let events = vec![
            (a, changed("w1")),
            (a, changed("w1")),
            (b, changed("w1")),
            (a, changed("w2")),
            (a, changed("w1")),
        ];
        assert_eq!(
            stale_workspaces(&events),
            vec![
                (a, "w1".to_string()),
                (b, "w1".to_string()),
                (a, "w2".to_string()),
            ],
            "one pull per (machine, workspace), in the order they were heard"
        );
    }

    /// The same workspace id on two machines is two workspaces — pane ids and
    /// store keys are per machine, and merging them would refresh one window
    /// from another box's record.
    #[test]
    fn a_change_is_scoped_to_the_machine_that_reported_it() {
        let (a, b) = (host("ssh-alias:a"), host("ssh-alias:b"));
        let events = vec![
            (a, ControlEvent::WorkspaceChanged { id: "same".into() }),
            (b, ControlEvent::WorkspaceChanged { id: "same".into() }),
        ];
        assert_eq!(stale_workspaces(&events).len(), 2);
    }

    /// Every other event is somebody else's business. A takeover in particular
    /// must not also trigger a pull: it is handled on its own path, and the
    /// record has not changed.
    #[test]
    fn only_a_workspace_change_asks_for_a_re_read() {
        let a = host("ssh-alias:a");
        let events = vec![
            (
                a,
                ControlEvent::Preempted {
                    workspace: "w1".into(),
                    by: "desktop".into(),
                },
            ),
            (
                a,
                ControlEvent::PaneExited {
                    pane_id: 3,
                    code: None,
                },
            ),
        ];
        assert!(stale_workspaces(&events).is_empty());
    }

    // ── The read-only degrade, state by state ───────────────────

    /// The degrade in one table: which states are read-only, what the
    /// bottom line says, and what the strip offers to do about it.
    ///
    /// `Preempted` reads differently on purpose — "not connected" would be a
    /// lie, because the link is usually fine and the workspace is simply
    /// somebody else's now.
    #[test]
    fn every_state_says_what_it_means_for_the_keyboard() {
        let cases = [
            (RemoteStatus::Attached, true, None, None),
            (
                RemoteStatus::Disconnected,
                false,
                Some("Not connected — typing has no effect"),
                Some("Connect"),
            ),
            (
                RemoteStatus::Connecting,
                false,
                Some("Not connected — typing has no effect"),
                None,
            ),
            (
                RemoteStatus::Reconnecting { attempt: 2 },
                false,
                Some("Not connected — typing has no effect"),
                Some("Retry Now"),
            ),
            (
                RemoteStatus::Preempted {
                    by: "desktop".into(),
                },
                false,
                Some("Opened elsewhere — typing has no effect"),
                Some("Take Back"),
            ),
            (
                RemoteStatus::Failed("no route to host".into()),
                false,
                Some("Not connected — typing has no effect"),
                Some("Retry"),
            ),
        ];
        for (status, accepts, notice, action) in cases {
            assert_eq!(status.accepts_input(), accepts, "{status:?}");
            assert_eq!(status.input_notice(), notice, "{status:?}");
            assert_eq!(status.action_label(), action, "{status:?}");
        }
    }

    /// The two states M6 added still produce a strip line, and the takeover one
    /// names the machine that took it — 状态条写"已在 <主机名> 上打开".
    #[test]
    fn the_new_states_name_what_happened() {
        assert_eq!(
            RemoteStatus::Reconnecting { attempt: 0 }
                .strip_message("build-box")
                .as_deref(),
            Some("Reconnecting to build-box…"),
            "the first attempt does not need a count"
        );
        assert_eq!(
            RemoteStatus::Reconnecting { attempt: 3 }
                .strip_message("build-box")
                .as_deref(),
            Some("Reconnecting to build-box… (attempt 4)")
        );
        assert_eq!(
            RemoteStatus::Preempted {
                by: "desktop".into()
            }
            .strip_message("build-box")
            .as_deref(),
            Some("This workspace was opened on desktop")
        );
    }

    // ── A deliberate disconnect ──────────────────────────────────────────────

    fn machine(alias: &str) -> (HostId, RemoteTarget) {
        let target = RemoteTarget::Alias {
            alias: alias.to_string(),
        };
        (target.host_id(), target)
    }

    /// A disconnect holds only while the machine still has a window on it.
    ///
    /// The supervisor reconnects to every machine with an open workspace, so
    /// without this set a disconnect would last one tick. With it kept
    /// *forever*, the opposite failure: a workspace closed and reopened a day
    /// later would come up offline against a decision the user has no memory of
    /// and nothing on screen to explain.
    #[test]
    fn a_disconnect_ends_when_the_last_window_on_that_machine_closes() {
        let (build, build_t) = machine("build-box");
        let (gpu, gpu_t) = machine("gpu-lab");
        let mut suspended = std::collections::HashSet::from([build, gpu]);

        // Both still have a window: both decisions still mean something.
        prune_suspended(&mut suspended, &[(build, build_t.clone()), (gpu, gpu_t)]);
        assert_eq!(suspended.len(), 2);

        // The gpu box's last workspace closed. Its disconnect goes with it; the
        // build box's is untouched.
        prune_suspended(&mut suspended, &[(build, build_t)]);
        assert_eq!(
            suspended.into_iter().collect::<Vec<_>>(),
            vec![build],
            "closing one machine's window must not resume another"
        );
    }

    /// Disconnecting drops the machine's link state, which *is* how the window
    /// says "not connected": `status_of` reads a missing `MachineLink` as
    /// `Disconnected`, and that state already draws the Connect button. Asking
    /// to reconnect then clears the decision, or the supervisor would undo the
    /// reconnect on its next tick.
    #[gpui::test]
    fn disconnecting_rests_at_not_connected_and_connect_undoes_it(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::core::config::pin_test_config_dir();
            cx.set_global(crate::core::config::Config::default());
            crate::ui::windows::WindowRegistry::init(cx);

            let (host, target) = machine("build-box");
            let mut entry = crate::core::session::Workspace::on_remote(RemoteRef::new(
                target,
                WorkspaceId::new(),
            ));
            entry.open = true;
            let id = entry.id;
            WorkspaceStore::install_for_test(
                cx,
                crate::core::session::Workspaces {
                    workspaces: vec![entry],
                    active: None,
                },
            );

            RemoteLinks::disconnect(cx, host);
            assert!(cx.default_global::<RemoteLinks>().suspended.contains(&host));
            assert_eq!(
                RemoteLinks::status_of(cx, id),
                Some(RemoteStatus::Disconnected),
                "a disconnected machine rests where a never-connected one does"
            );

            // The strip's Connect button.
            RemoteLinks::retry_now(cx, id);
            assert!(
                !cx.default_global::<RemoteLinks>().suspended.contains(&host),
                "asking to connect must outrank having asked to disconnect"
            );
        });
    }
}

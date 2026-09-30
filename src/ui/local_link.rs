use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{App, Global};
use tty7_core::daemon::control::{ControlClient, ControlRequest, ReplyOk};

use crate::core::session::{WorkspaceId, WorkspaceStore};
use crate::ui::remote_workspace::{Backoff, RemoteStatus};

#[derive(Default)]
pub struct LocalLink {
    client: Option<Arc<ControlClient>>,
    /// The daemon process the current link is talking to, by its hello
    /// instance — empty until the first connect records one. Survives
    /// `invalidate` on purpose: forgetting it there would make every
    /// reconnect a first sighting, and a daemon that came back as a
    /// *different* process would never be noticed (#553).
    instance: String,
    backoff: Backoff,
    next_attempt: Option<std::time::Instant>,
    attempting: bool,
    pumping: bool,
    /// Which of this computer's workspaces the windows here are driving, as far
    /// as the daemon is concerned — see [`Claims`].
    claims: Claims,
}

impl Global for LocalLink {}

impl LocalLink {
    pub fn install(cx: &mut App) {
        crate::ui::remote_workspace::install_event_observer();
        let link = cx.default_global::<LocalLink>();
        if link.pumping {
            return;
        }
        link.pumping = true;
        cx.spawn(async move |cx| {
            loop {
                cx.update(|cx| {
                    Self::tick(cx);
                    crate::ui::remote_workspace::drain_events(cx);
                    pump_claims(cx);
                });
                cx.background_executor()
                    .timer(crate::ui::remote_workspace::PUMP_TICK)
                    .await;
            }
        })
        .detach();
    }

    pub fn client(cx: &mut App) -> Option<Arc<ControlClient>> {
        let link = cx.default_global::<LocalLink>();
        link.client.as_ref().filter(|c| c.is_connected()).cloned()
    }

    /// Whether the local daemon advertised `feature` on its control hello —
    /// [`HostLinks::peer_supports`](crate::ui::remote_connect::HostLinks::peer_supports)
    /// for this machine. `false` while the link is down, for the same reason.
    pub fn supports(cx: &App, feature: &str) -> bool {
        cx.try_global::<LocalLink>()
            .and_then(|link| link.client.as_ref())
            .is_some_and(|c| c.is_connected() && c.hello().has_feature(feature))
    }

    /// Drops the cached client without waiting for its reader to notice.
    ///
    /// `ControlClient::is_connected` only flips once the reader sees EOF, so
    /// for a moment after we kill the daemon ourselves the dead link still
    /// hands itself out and every call on it fails. Callers that know the far
    /// end is gone say so here, and the next tick reconnects.
    ///
    /// The remembered hello `instance` deliberately survives: the reconnect
    /// compares against it to tell "same daemon, link hiccuped" from "new
    /// daemon process" (#553), and forgetting it here — the restart path's
    /// own first move — would blind exactly that comparison.
    pub fn invalidate(cx: &mut App) {
        let link = cx.default_global::<LocalLink>();
        if link.client.take().is_some() {
            log::info!("dropped the control link to the local daemon; it was restarted");
        }
        link.backoff.reset();
        link.next_attempt = None;
    }

    fn tick(cx: &mut App) {
        let now = std::time::Instant::now();
        let link = cx.default_global::<LocalLink>();
        if link.attempting {
            return;
        }
        if let Some(client) = &link.client {
            if client.is_connected() {
                return;
            }
            log::info!("lost the control link to the local daemon; reconnecting");
            link.client = None;
        }
        match due(
            link.next_attempt,
            link.backoff.attempt(),
            link.backoff.delay(),
            now,
        ) {
            Due::Now => {}
            Due::Wait => return,
            Due::ScheduleAt(at) => {
                link.next_attempt = Some(at);
                return;
            }
        }
        link.next_attempt = None;
        link.attempting = true;
        let _ = link.backoff.advance();

        cx.spawn(async move |cx| {
            let connected = cx
                .background_executor()
                .spawn(async move { connect_blocking() })
                .await;
            cx.update(|cx| {
                let link = cx.default_global::<LocalLink>();
                link.attempting = false;
                match connected {
                    Ok(client) => {
                        log::info!("control link to the local daemon is up");
                        // A daemon that died and came back is a *new process*
                        // whose registry knows nothing about the panes this
                        // window is showing — and from the client's side a
                        // killed daemon is indistinguishable from one whose
                        // shells all exited at once (its DeathReporter says
                        // nothing while it shuts down, and a kill says nothing
                        // ever), so the panes on screen are probably lying
                        // about being alive. The instance id in the hello is
                        // the only way to tell "same daemon, link hiccuped"
                        // from "new daemon": compare before syncing, or
                        // `on_link_up` would push the window of dead panes up
                        // as the new daemon's truth (#553).
                        let restarted = {
                            let link = cx.default_global::<LocalLink>();
                            crate::ui::tree_sync::note_instance(
                                &mut link.instance,
                                &client.hello().instance,
                            )
                        };
                        let link = cx.default_global::<LocalLink>();
                        link.client = Some(client);
                        link.backoff.reset();
                        link.next_attempt = None;
                        // Whatever this process held went down with the old
                        // connection: the daemon releases a connection's
                        // workspaces when it ends. The pump claims the ones
                        // still legitimately ours again over this link, and
                        // leaves the ones someone took from us alone.
                        link.claims.new_link();
                        crate::ui::machine_mirror::MachineMirrors::refresh(
                            cx,
                            tty7_core::host::HostId::LOCAL,
                        );
                        if restarted {
                            // The link installed just above is this new
                            // daemon's own and answers right now, so the pull
                            // goes out on it. Dropping it first — which is what
                            // a caller that killed the daemon itself has to do —
                            // would leave every window waiting out another
                            // connect, and a pull that runs out its fifteen
                            // seconds waiting owes a `Replace` a window with
                            // tabs on screen never claims back.
                            crate::ui::tree_sync::resync_local_windows_from_tree(cx);
                        } else {
                            crate::ui::tree_sync::on_link_up(cx, tty7_core::host::HostId::LOCAL);
                        }
                    }
                    Err(e) => match dialect_refusal(&e) {
                        // Retrying will not talk this server round, and the
                        // window already on screen has no tabs to show. Arm the
                        // prompt so the next window built offers the restart.
                        Some(refusal) => {
                            log::warn!(
                                "the local server refused this build's control dialect: {e}"
                            );
                            crate::daemon::spawn::note_daemon_mismatch(
                                crate::daemon::spawn::DaemonMismatch::Dialect(refusal),
                            );
                        }
                        None => log::debug!("local control link attempt failed: {e}"),
                    },
                }
            });
        })
        .detach();
    }
}

// ---------------------------------------------------------------------------
// Workspace claims
//
// One workspace is driven by one window at a time, wherever that window runs.
// Remote clients have always told the daemon which workspaces they hold
// (`WorkspaceAttach`), and the daemon has always told the displaced holder
// (`ControlEvent::Preempted`). The windows on the daemon's own machine used to
// skip both halves, so a remote client and a local window could drive the same
// panes at once — each resizing them to its own grid, the last resize winning.
//
// The rules the code below keeps:
//
// - A local window that shows a workspace claims it. Opened on purpose
//   (switcher, `tty7 open`, a new window onto it) the claim takes the workspace
//   over from whoever holds it. Arrived at any other way — session restore, a
//   relaunch, a reconnect — it only claims a workspace nobody else is holding,
//   and otherwise starts out taken over. Nothing but an explicit act ever takes
//   a workspace back, so two clients never trade it back and forth.
// - Taken over, the window lets go of its panes (detaching, never killing) and
//   says who has it, with a Take Back button: the remote strip, reused.
// ---------------------------------------------------------------------------

/// How a claim asks for a workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    /// Take it, whoever holds it now. Only for something the user did.
    Take,
    /// Claim it only if nobody else is holding it; otherwise yield.
    IfFree,
}

/// What a claim came back with.
#[derive(Debug)]
enum ClaimOutcome {
    Attached {
        took_over_from: Option<String>,
    },
    /// An `IfFree` claim found another client holding the workspace.
    HeldBy(String),
    Failed(String),
}

/// What [`Claims::finish`] made of a claim that came back.
#[derive(Debug, PartialEq, Eq)]
enum Settled {
    /// This window holds the workspace now.
    Held,
    /// ...and it was a Take Back, so the window is rebuilt from the tree.
    Reclaimed,
    /// Someone else has it; the window has to let go of its panes.
    Preempted,
    /// The daemon refused. A Take Back that failed goes back to taken over.
    Refused,
    /// The answer came over a link that is gone; the claim died with it.
    Stale,
}

/// What a hydration that just pulled a local workspace's layout should do.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    Proceed,
    /// Leave the panes alone. `newly` when this is what found out that the
    /// workspace is somebody else's.
    Yield {
        newly: bool,
    },
}

/// The local window's side of workspace attachment, the same bookkeeping
/// `RemoteLinks` keeps for a remote machine. Kept apart from it because the
/// remote supervisor forgets everything the moment no remote workspace is open,
/// and it keys its state by machine, of which this is the one it never serves.
///
/// Pure state: nothing here touches a window or the wire, which is what lets
/// the rules be tested on their own.
#[derive(Default)]
struct Claims {
    /// Bumped per link. A claim carries the generation it went out on, so an
    /// answer arriving over a dead link is not taken as a claim on the new one.
    generation: u64,
    /// Workspaces a claim has gone out for over the current link, answered or
    /// refused. Refusals count, or a refused claim would be resent every tick.
    sent: HashSet<WorkspaceId>,
    /// Claims on the wire right now.
    attaching: HashSet<WorkspaceId>,
    /// Taken over, with the name of the client that has them.
    preempted: HashMap<WorkspaceId, String>,
    /// A Take Back on the wire, still carrying who it is being taken from so a
    /// refusal can put the takeover back.
    reclaiming: HashMap<WorkspaceId, String>,
    /// Opened on purpose: the first claim takes it over from anyone.
    explicit: HashSet<WorkspaceId>,
}

impl Claims {
    fn new_link(&mut self) {
        self.generation += 1;
        self.sent.clear();
    }

    /// Drops everything about workspaces no window here shows any more, and
    /// returns the ones this process had claimed so the daemon can be told to
    /// let them go.
    fn prune(&mut self, open: &HashSet<WorkspaceId>) -> Vec<WorkspaceId> {
        let released: Vec<WorkspaceId> = self
            .sent
            .iter()
            .filter(|ws| !open.contains(ws) && !self.preempted.contains_key(ws))
            .copied()
            .collect();
        self.sent.retain(|ws| open.contains(ws));
        self.preempted.retain(|ws, _| open.contains(ws));
        self.reclaiming.retain(|ws, _| open.contains(ws));
        self.explicit.retain(|ws| open.contains(ws));
        released
    }

    /// The claims to send now, each marked in flight on the way out.
    ///
    /// A workspace someone else holds is left out: taking it back is the
    /// user's call, never the pump's.
    fn due(&mut self, open: &HashSet<WorkspaceId>) -> Vec<(WorkspaceId, Claim)> {
        let mut due = Vec::new();
        for &ws in open {
            if self.attaching.contains(&ws) || self.preempted.contains_key(&ws) {
                continue;
            }
            let claim = if self.reclaiming.contains_key(&ws) {
                Claim::Take
            } else if self.sent.contains(&ws) {
                continue;
            } else if self.explicit.contains(&ws) {
                Claim::Take
            } else {
                Claim::IfFree
            };
            due.push((ws, claim));
        }
        for (ws, _) in &due {
            self.attaching.insert(*ws);
        }
        due
    }

    fn finish(&mut self, ws: WorkspaceId, generation: u64, outcome: ClaimOutcome) -> Settled {
        self.attaching.remove(&ws);
        if generation != self.generation {
            return Settled::Stale;
        }
        self.sent.insert(ws);
        match outcome {
            ClaimOutcome::Attached { .. } => {
                self.explicit.remove(&ws);
                match self.reclaiming.remove(&ws) {
                    Some(_) => Settled::Reclaimed,
                    None => Settled::Held,
                }
            }
            ClaimOutcome::HeldBy(by) => {
                self.preempt(ws, by);
                Settled::Preempted
            }
            ClaimOutcome::Failed(_) => {
                if let Some(by) = self.reclaiming.remove(&ws) {
                    self.preempted.insert(ws, by);
                }
                Settled::Refused
            }
        }
    }

    fn preempt(&mut self, ws: WorkspaceId, by: String) {
        self.reclaiming.remove(&ws);
        self.explicit.remove(&ws);
        self.preempted.insert(ws, by);
    }

    fn take_back(&mut self, ws: WorkspaceId) -> bool {
        match self.preempted.remove(&ws) {
            Some(by) => {
                self.reclaiming.insert(ws, by);
                true
            }
            None => false,
        }
    }

    fn status(&self, ws: WorkspaceId) -> Option<RemoteStatus> {
        // Same reading as `RemoteLinks::status_of`: a Take Back on the wire is
        // neither taken over nor attached, and Connecting is the state with no
        // button, so the same reclaim cannot be started twice.
        if self.reclaiming.contains_key(&ws) {
            return Some(RemoteStatus::Connecting);
        }
        self.preempted
            .get(&ws)
            .map(|by| RemoteStatus::Preempted { by: by.clone() })
    }

    /// Whether a hydration may put `ws`'s panes up, given who the daemon says
    /// holds it. `me` is this machine's name, which is also what a connection
    /// of ours the daemon has not noticed is gone yet would be holding it as.
    fn gate(&mut self, ws: WorkspaceId, holder: Option<&str>, me: &str) -> Gate {
        if self.preempted.contains_key(&ws) {
            return Gate::Yield { newly: false };
        }
        if self.reclaiming.contains_key(&ws) || self.explicit.contains(&ws) {
            return Gate::Proceed;
        }
        match holder {
            Some(by) if !by.is_empty() && by != me => {
                self.preempt(ws, by.to_string());
                Gate::Yield { newly: true }
            }
            _ => Gate::Proceed,
        }
    }
}

/// The local workspaces some window in this process is showing.
fn open_local_workspaces(cx: &mut App) -> HashSet<WorkspaceId> {
    if !cx.has_global::<crate::ui::windows::WindowRegistry>() || !cx.has_global::<WorkspaceStore>()
    {
        return HashSet::new();
    }
    crate::ui::windows::WindowRegistry::open_windows(cx)
        .into_iter()
        .map(|(ws, _)| ws)
        .filter(|ws| WorkspaceStore::host_of(cx, *ws).is_local())
        .collect()
}

/// The local link, if it is up and its daemon keeps attachments at all. One
/// that serves no machine tree refuses every attach, so there is nothing to
/// claim and nobody to be taken over by.
fn claim_client(cx: &mut App) -> Option<Arc<ControlClient>> {
    LocalLink::client(cx).filter(|c| {
        c.hello()
            .has_feature(tty7_core::daemon::control::feature::MACHINE_TREE)
    })
}

fn pump_claims(cx: &mut App) {
    let open = open_local_workspaces(cx);
    let client = claim_client(cx);
    let (released, due, generation) = {
        let claims = &mut cx.default_global::<LocalLink>().claims;
        let released = claims.prune(&open);
        let due = match client {
            Some(_) => claims.due(&open),
            None => Vec::new(),
        };
        (released, due, claims.generation)
    };
    let Some(client) = client else {
        return;
    };
    for ws in released {
        let client = Arc::clone(&client);
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = client.call(ControlRequest::WorkspaceDetach { id: ws.to_string() })
                {
                    log::debug!("could not let go of workspace {ws}: {e}");
                }
            })
            .detach();
    }
    for (ws, claim) in due {
        let client = Arc::clone(&client);
        // Never on the UI thread: the request's deadline is ten seconds.
        cx.spawn(async move |cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { claim_blocking(&client, ws, claim) })
                .await;
            cx.update(|cx| finish_claim(cx, ws, generation, outcome));
        })
        .detach();
    }
}

fn claim_blocking(client: &ControlClient, ws: WorkspaceId, claim: Claim) -> ClaimOutcome {
    if claim == Claim::IfFree {
        // The daemon answers a workspace's holder by name. A race with another
        // client claiming it between this read and the attach below is possible
        // and harmless: that client is told it was taken over, and keeps its
        // Take Back.
        let me = crate::ui::remote_connect::client_hostname();
        if let Ok(ReplyOk::WorkspaceTree(tree)) =
            client.call(ControlRequest::WorkspaceTree { workspace: ws })
            && let Some(holder) = tree.attachment
            && !holder.hostname.is_empty()
            && holder.hostname != me
        {
            return ClaimOutcome::HeldBy(holder.hostname);
        }
    }
    match client.call(ControlRequest::WorkspaceAttach { id: ws.to_string() }) {
        Ok(ReplyOk::Attached { took_over_from }) => ClaimOutcome::Attached { took_over_from },
        Ok(other) => ClaimOutcome::Failed(format!("WorkspaceAttach answered {other:?}")),
        Err(e) => ClaimOutcome::Failed(e.to_string()),
    }
}

fn finish_claim(cx: &mut App, ws: WorkspaceId, generation: u64, outcome: ClaimOutcome) {
    match &outcome {
        ClaimOutcome::Attached {
            took_over_from: Some(who),
        } => log::info!("workspace {ws} taken over from {who}"),
        ClaimOutcome::Attached { .. } => log::debug!("workspace {ws} claimed"),
        ClaimOutcome::HeldBy(who) => {
            log::info!("workspace {ws} is being driven by {who}; leaving it to them")
        }
        ClaimOutcome::Failed(e) => log::warn!("could not claim workspace {ws}: {e}"),
    }
    let settled = cx
        .default_global::<LocalLink>()
        .claims
        .finish(ws, generation, outcome);
    match settled {
        // The other client had this workspace for a while and may have moved
        // every pane in it. Only the tree knows what it looks like now.
        Settled::Reclaimed => crate::ui::tree_sync::resync_window_from_tree(cx, ws),
        Settled::Preempted => let_go(cx, ws),
        Settled::Held | Settled::Refused | Settled::Stale => {}
    }
    cx.refresh_windows();
}

/// The window on `ws` stops driving it: its panes detach — the processes in
/// them run on, untouched — and its layout sync stands down until a Take Back.
fn let_go(cx: &mut App, ws: WorkspaceId) {
    crate::ui::remote_workspace::release_panes(cx, ws);
    crate::ui::tree_sync::on_preempted(cx, ws);
    cx.refresh_windows();
}

impl LocalLink {
    /// The daemon says another client took `key` from this process.
    pub(crate) fn on_preempted(cx: &mut App, key: &str, by: String) {
        let Ok(ws) = key.parse::<WorkspaceId>() else {
            log::warn!("preempted on this computer for a workspace key {key:?} that is not one");
            return;
        };
        if !open_local_workspaces(cx).contains(&ws) {
            log::debug!("preempted on this computer for workspace {ws}, which no window shows");
            return;
        }
        log::info!("workspace {ws} was taken over by {by}");
        cx.default_global::<LocalLink>().claims.preempt(ws, by);
        let_go(cx, ws);
    }

    /// The user opened `ws` on purpose, so the claim that follows takes it
    /// over from whoever has it. Only for a window that is about to *start*
    /// showing it: focusing a window already on it is not taking anything.
    pub(crate) fn open_explicitly(cx: &mut App, ws: WorkspaceId) {
        if !cx.has_global::<WorkspaceStore>() || !WorkspaceStore::host_of(cx, ws).is_local() {
            return;
        }
        let claims = &mut cx.default_global::<LocalLink>().claims;
        // Left and came straight back before the pump forgot the takeover:
        // coming back on purpose is a Take Back.
        if !claims.take_back(ws) {
            claims.explicit.insert(ws);
        }
    }

    /// Take Back: the one thing that reclaims a workspace someone took over.
    pub(crate) fn take_back(cx: &mut App, ws: WorkspaceId) {
        if cx.default_global::<LocalLink>().claims.take_back(ws) {
            log::info!("taking workspace {ws} back");
        }
        cx.refresh_windows();
    }

    /// The strip's account of a local workspace, `None` while this window is
    /// the one driving it.
    pub(crate) fn status_of(cx: &App, ws: WorkspaceId) -> Option<RemoteStatus> {
        cx.try_global::<LocalLink>()?.claims.status(ws)
    }

    pub(crate) fn is_preempted(cx: &App, ws: WorkspaceId) -> bool {
        cx.try_global::<LocalLink>()
            .is_some_and(|link| link.claims.preempted.contains_key(&ws))
    }

    /// The local workspaces another client is driving, for the switcher.
    pub(crate) fn preempted(cx: &App) -> Vec<WorkspaceId> {
        cx.try_global::<LocalLink>()
            .map(|link| link.claims.preempted.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Called by a hydration of a local workspace with the holder the pulled
    /// tree names. `false` means the window must not put the panes up: the
    /// workspace is somebody else's, and attaching to its panes — even for the
    /// moment before being told — would resize them under the client driving
    /// them.
    pub(crate) fn hydration_may_proceed(
        cx: &mut App,
        ws: WorkspaceId,
        holder: Option<&str>,
    ) -> bool {
        let me = crate::ui::remote_connect::client_hostname();
        let gate = cx
            .default_global::<LocalLink>()
            .claims
            .gate(ws, holder, &me);
        match gate {
            Gate::Proceed => true,
            Gate::Yield { newly } => {
                if newly {
                    log::info!(
                        "workspace {ws} is being driven by {}; opening it taken over",
                        holder.unwrap_or_default()
                    );
                    let_go(cx, ws);
                }
                false
            }
        }
    }
}

/// What a tick should do about reconnecting.
#[derive(Debug, PartialEq, Eq)]
enum Due {
    /// Try now.
    Now,
    /// Something is already scheduled and is not due yet.
    Wait,
    /// Nothing was scheduled; put the next attempt here and come back.
    ScheduleAt(std::time::Instant),
}

/// The reconnect schedule, with the clock and the link's state passed in.
///
/// The very first attempt goes out immediately — at startup the daemon is
/// usually seconds from being up, and making the window wait a backoff for the
/// first try would be a visible stall — and only from the second does the
/// backoff get a say.
///
/// Nothing here reads a global or the wall clock, which is what lets it be
/// tested. The structurally identical scheduler in `remote_workspace` is
/// covered through a `TestAppContext`; this one, which every user depends on at
/// launch, was covered not at all.
fn due(
    scheduled: Option<std::time::Instant>,
    attempts_so_far: u32,
    delay: std::time::Duration,
    now: std::time::Instant,
) -> Due {
    match scheduled {
        None if attempts_so_far == 0 => Due::Now,
        None => Due::ScheduleAt(now + delay),
        Some(at) if at > now => Due::Wait,
        Some(_) => Due::Now,
    }
}

fn connect_blocking() -> std::io::Result<Arc<ControlClient>> {
    use tty7_core::daemon::control::ControlHello;

    crate::daemon::spawn::ensure_running().map_err(std::io::Error::other)?;
    // The machine's real name, not "this computer": it is what a remote client
    // this window takes a workspace from is told ("opened on …"), and from over
    // there "this computer" names the wrong machine.
    let hello = ControlHello::gui(
        uuid::Uuid::new_v4().to_string(),
        crate::ui::remote_connect::client_hostname(),
    );
    let sink: tty7_core::daemon::control::EventSink = Box::new(local_event_sink);
    #[cfg(unix)]
    let client = {
        let stream = std::os::unix::net::UnixStream::connect(
            tty7_core::host::server::control_socket_path()?,
        )?;
        // Every other client socket goes through `tune` on its way up — 256 KiB
        // buffers on Unix, nodelay on Windows — because it is `connect_endpoint`
        // that calls it, and this is the one connect that does not go through
        // there. It is also the busiest: the control link carries every event
        // the window redraws from.
        tty7_core::daemon::transport::tune(&stream);
        ControlClient::over_unix(stream, &hello, sink)?
    };
    #[cfg(windows)]
    let client =
        ControlClient::over_tcp(tty7_core::host::server::connect_control()?, &hello, sink)?;
    Ok(Arc::new(client))
}

/// The dialect mismatch behind a failed connect, if that is what it was.
///
/// It arrives as the handshake's own wording rather than a typed error, so this
/// is where the string becomes the fact again.
fn dialect_refusal(e: &std::io::Error) -> Option<tty7_core::daemon::control::DialectRefusal> {
    tty7_core::daemon::control::parse_dialect_refusal(&e.to_string())
}

fn local_event_sink(event: tty7_core::daemon::control::ControlEvent) {
    tty7_core::daemon::control::observe_event(tty7_core::host::HostId::LOCAL, event);
}

#[cfg(test)]
mod schedule_tests {
    use super::*;
    use std::time::{Duration, Instant};

    const D: Duration = Duration::from_secs(2);

    #[test]
    fn the_very_first_attempt_goes_out_immediately() {
        // No backoff before anything has failed: at launch the daemon is
        // seconds from being up and a delay here is a visible stall.
        assert_eq!(due(None, 0, D, Instant::now()), Due::Now);
    }

    #[test]
    fn a_later_attempt_with_nothing_scheduled_gets_scheduled() {
        let now = Instant::now();
        assert_eq!(due(None, 1, D, now), Due::ScheduleAt(now + D));
        assert_eq!(
            due(None, 7, Duration::from_secs(30), now),
            Due::ScheduleAt(now + Duration::from_secs(30)),
            "the delay is the backoff's to decide, not this function's"
        );
    }

    #[test]
    fn a_scheduled_attempt_in_the_future_waits() {
        let now = Instant::now();
        assert_eq!(due(Some(now + D), 3, D, now), Due::Wait);
    }

    #[test]
    fn a_scheduled_attempt_that_has_come_due_fires() {
        let now = Instant::now();
        assert_eq!(due(Some(now - D), 3, D, now), Due::Now);
        assert_eq!(
            due(Some(now), 3, D, now),
            Due::Now,
            "exactly due counts as due, or a tick landing on the instant waits a whole round"
        );
    }

    /// Scheduling happens once. A tick that arrives while an attempt is
    /// pending must not push the deadline further out, or a busy window would
    /// starve the reconnect forever.
    #[test]
    fn ticking_repeatedly_does_not_move_a_pending_deadline() {
        let start = Instant::now();
        let at = start + D;
        for step in [0u64, 1, 100, 500] {
            let now = start + Duration::from_millis(step);
            assert_eq!(due(Some(at), 3, D, now), Due::Wait, "at +{step}ms");
        }
        assert_eq!(due(Some(at), 3, D, start + D), Due::Now);
    }
}

#[cfg(test)]
mod claims_tests {
    use super::*;

    fn open(ids: &[WorkspaceId]) -> HashSet<WorkspaceId> {
        ids.iter().copied().collect()
    }

    fn attached() -> ClaimOutcome {
        ClaimOutcome::Attached {
            took_over_from: None,
        }
    }

    /// Claims `ws` over the current link the way the pump would, and lands it.
    fn hold(claims: &mut Claims, ws: WorkspaceId) {
        let due = claims.due(&open(&[ws]));
        assert_eq!(due.len(), 1);
        let generation = claims.generation;
        assert_eq!(claims.finish(ws, generation, attached()), Settled::Held);
    }

    #[test]
    fn a_window_showing_a_workspace_claims_it() {
        let mut claims = Claims::default();
        let ws = WorkspaceId::new();
        assert_eq!(claims.due(&open(&[ws])), vec![(ws, Claim::IfFree)]);
        assert!(
            claims.due(&open(&[ws])).is_empty(),
            "a claim on the wire is not sent again every tick"
        );
        assert_eq!(
            claims.finish(ws, claims.generation, attached()),
            Settled::Held
        );
        assert!(
            claims.due(&open(&[ws])).is_empty(),
            "held is held until the link changes"
        );
        assert_eq!(claims.status(ws), None, "this window drives it: no strip");
    }

    #[test]
    fn opening_a_workspace_on_purpose_takes_it_over() {
        let mut claims = Claims::default();
        let ws = WorkspaceId::new();
        claims.explicit.insert(ws);
        assert_eq!(claims.due(&open(&[ws])), vec![(ws, Claim::Take)]);
        assert_eq!(
            claims.finish(
                ws,
                claims.generation,
                ClaimOutcome::Attached {
                    took_over_from: Some("laptop".into()),
                },
            ),
            Settled::Held
        );
        assert!(
            !claims.explicit.contains(&ws),
            "the takeover is spent; a later reconnect claims politely"
        );
    }

    #[test]
    fn a_remote_client_taking_the_workspace_puts_the_window_in_taken_over() {
        let mut claims = Claims::default();
        let ws = WorkspaceId::new();
        hold(&mut claims, ws);

        claims.preempt(ws, "laptop".into());
        assert_eq!(
            claims.status(ws),
            Some(RemoteStatus::Preempted {
                by: "laptop".into()
            })
        );
        assert!(!claims.status(ws).unwrap().accepts_input());
        assert!(
            claims.due(&open(&[ws])).is_empty(),
            "a workspace someone else holds is never reclaimed by the pump"
        );
        assert_eq!(
            claims.gate(ws, Some("laptop"), "studio"),
            Gate::Yield { newly: false },
            "nor rebuilt under the client that has it"
        );
    }

    /// Restore, relaunch, reconnect: a workspace another client holds is not
    /// claimed from under it. The window opens taken over.
    #[test]
    fn a_restored_window_yields_a_workspace_someone_else_holds() {
        let mut claims = Claims::default();
        let ws = WorkspaceId::new();

        assert_eq!(claims.due(&open(&[ws])), vec![(ws, Claim::IfFree)]);
        assert_eq!(
            claims.finish(ws, claims.generation, ClaimOutcome::HeldBy("laptop".into())),
            Settled::Preempted
        );
        assert_eq!(
            claims.status(ws),
            Some(RemoteStatus::Preempted {
                by: "laptop".into()
            })
        );
        for _ in 0..3 {
            claims.new_link();
            assert!(
                claims.due(&open(&[ws])).is_empty(),
                "no reconnect ever takes it back on its own"
            );
        }
    }

    #[test]
    fn a_hydration_yields_to_another_holder_unless_the_user_asked() {
        let mut claims = Claims::default();
        let (restored, asked, free, stale_self) = (
            WorkspaceId::new(),
            WorkspaceId::new(),
            WorkspaceId::new(),
            WorkspaceId::new(),
        );
        assert_eq!(
            claims.gate(restored, Some("laptop"), "studio"),
            Gate::Yield { newly: true }
        );
        assert!(claims.preempted.contains_key(&restored));

        claims.explicit.insert(asked);
        assert_eq!(claims.gate(asked, Some("laptop"), "studio"), Gate::Proceed);

        assert_eq!(claims.gate(free, None, "studio"), Gate::Proceed);
        assert_eq!(
            claims.gate(stale_self, Some("studio"), "studio"),
            Gate::Proceed,
            "this machine's own name is a connection of ours the daemon has not dropped yet"
        );
    }

    /// A link that comes back re-claims what the window was holding, and only
    /// that: a workspace it had been pushed off stays with whoever pushed.
    #[test]
    fn a_reconnect_reclaims_held_workspaces_and_only_those() {
        let mut claims = Claims::default();
        let (held, taken) = (WorkspaceId::new(), WorkspaceId::new());
        hold(&mut claims, held);
        hold(&mut claims, taken);
        claims.preempt(taken, "laptop".into());

        claims.new_link();
        assert_eq!(
            claims.due(&open(&[held, taken])),
            vec![(held, Claim::IfFree)],
            "held comes back, politely in case someone took it while the link was down"
        );
    }

    #[test]
    fn an_answer_over_a_dead_link_does_not_count_on_the_new_one() {
        let mut claims = Claims::default();
        let ws = WorkspaceId::new();
        let _ = claims.due(&open(&[ws]));
        let old = claims.generation;
        claims.new_link();
        assert_eq!(
            claims.finish(ws, old, ClaimOutcome::Failed("link closed".into())),
            Settled::Stale
        );
        assert_eq!(
            claims.due(&open(&[ws])),
            vec![(ws, Claim::IfFree)],
            "the new link still owes the claim"
        );
    }

    #[test]
    fn take_back_reclaims_and_a_refusal_puts_the_takeover_back() {
        let mut claims = Claims::default();
        let ws = WorkspaceId::new();
        hold(&mut claims, ws);
        claims.preempt(ws, "laptop".into());

        assert!(claims.take_back(ws));
        assert_eq!(
            claims.status(ws),
            Some(RemoteStatus::Connecting),
            "no second Take Back button while the first is on the wire"
        );
        assert_eq!(claims.due(&open(&[ws])), vec![(ws, Claim::Take)]);
        assert_eq!(
            claims.finish(ws, claims.generation, ClaimOutcome::Failed("busy".into())),
            Settled::Refused
        );
        assert_eq!(
            claims.status(ws),
            Some(RemoteStatus::Preempted {
                by: "laptop".into()
            })
        );
        assert!(claims.due(&open(&[ws])).is_empty(), "and it is not retried");

        assert!(claims.take_back(ws));
        let _ = claims.due(&open(&[ws]));
        assert_eq!(
            claims.finish(ws, claims.generation, attached()),
            Settled::Reclaimed,
            "a landed Take Back rebuilds the window from the tree"
        );
        assert_eq!(claims.status(ws), None);
        assert!(!claims.take_back(ws), "nothing left to take back");
    }

    #[test]
    fn a_closed_window_lets_go_of_what_it_held() {
        let mut claims = Claims::default();
        let (kept, closed, lost) = (WorkspaceId::new(), WorkspaceId::new(), WorkspaceId::new());
        hold(&mut claims, kept);
        hold(&mut claims, closed);
        hold(&mut claims, lost);
        claims.preempt(lost, "laptop".into());

        assert_eq!(
            claims.prune(&open(&[kept])),
            vec![closed],
            "only what this window still held is detached; a taken-over one is not ours"
        );
        assert!(!claims.preempted.contains_key(&lost));
        assert!(claims.due(&open(&[kept])).is_empty());
    }

    #[gpui::test]
    fn a_taken_over_local_workspace_refuses_input(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let ws = WorkspaceId::new();
            assert!(crate::ui::remote_workspace::workspace_accepts_input(cx, ws));
            cx.default_global::<LocalLink>()
                .claims
                .preempt(ws, "laptop".into());
            assert!(crate::ui::remote_workspace::workspace_is_preempted(cx, ws));
            assert!(!crate::ui::remote_workspace::workspace_accepts_input(
                cx, ws
            ));
            assert_eq!(LocalLink::preempted(cx), vec![ws]);

            LocalLink::take_back(cx, ws);
            assert!(
                !crate::ui::remote_workspace::workspace_is_preempted(cx, ws),
                "tree sync may run again once the Take Back is out"
            );
        });
    }
}

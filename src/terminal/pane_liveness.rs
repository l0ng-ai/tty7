//! Which of a workspace's saved panes are still running — asked of **the
//! machine that workspace lives on**, cached per machine, probed off the UI
//! thread.
//!
//! # The bug this exists to close
//!
//! A `pane_id` is minted by one daemon and means nothing to any other. Two
//! machines hand out `1`, `2`, `3` in the same order, so a remote workspace's
//! saved ids overlap this computer's almost perfectly. Every liveness question
//! that skipped the route therefore had *two* wrong answers available: the
//! benign one (the remote's ids are absent here, so its sessions read as
//! stopped) and the misleading one — the id happens to name a live *local*
//! pane, and a workspace on a box that has been off for a week lights up green
//! because somebody's shell on this laptop holds the number. The title-bar
//! workspace menu was doing exactly the latter.
//!
//! Routing alone does not fix a *cross-workspace* view. The picker and the
//! workspace menu list several workspaces at once, on several machines at once,
//! so there is no single route to send: it takes one query per machine, and
//! those queries cannot be waited on in turn from `render`.
//!
//! # Three states, not two
//!
//! | State | Means | Drawn as |
//! |---|---|---|
//! | [`Liveness::Alive`] | the machine answered, and it still has one of these panes | green corner dot |
//! | [`Liveness::Stopped`] | the machine answered, and none of them are left | no dot |
//! | [`Liveness::Unknown`] | we could not ask | muted corner dot |
//!
//! `Unknown` is the state remote workspaces made necessary. A failed query to
//! another machine is not evidence that anything died — the sessions are very
//! probably fine and the *link* is what broke — and rendering it as "stopped"
//! would tell the user their work is gone every time the network blinks.
//!
//! **A local `List` failing is not `Unknown`.** It travels a unix socket to a
//! daemon whose absence is itself the answer: no daemon, no live panes. So a
//! local host with no cached *liveness* answer reads `Stopped`, which is what
//! this page has always drawn — the async cache changes remote behaviour and
//! leaves local pixels alone.
//!
//! Not knowing which panes to ask about is a different thing, and it is
//! `Unknown` on every machine. The ids live in the machine's tree
//! ([`crate::ui::machine_mirror`]), so until that first pull lands there is no
//! question to put to the daemon — and "no ids yet" must not be read as "no
//! sessions", which is a claim about the user's work founded on our own
//! ignorance. Locally the pull lands within a frame or two of launch; where
//! there is no control link at all, a muted dot is exactly the truth.
//!
//! # How it is filled
//!
//! [`sweep`] is called from the render paths that show liveness. It never
//! blocks: it looks at the workspace list, and for each machine whose answer is
//! missing or past its TTL it starts one background query. All of them fly at
//! once — N machines cost one round trip, not N in a row — and [`InFlight`]
//! keeps a frame that re-asks before the answer lands from starting a second.
//! Landing goes through `update_global`, so the `observe_global` hook in
//! [`crate::ui::app`] repaints whatever is on screen.
//!
//! A machine this process has no connection to is **not** probed: asking would
//! mean dialling SSH, and a liveness dot is not a reason to open a connection
//! (or raise a passphrase prompt). It stays `Unknown`, which is the truth.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, BorrowAppContext as _};

use crate::core::session::{WindowView, WorkspaceId, WorkspaceStore};
use crate::terminal::{PaneRoute, RemoteTerminal};
use crate::ui::host_ops::{HostId, InFlight};

/// How long this machine's answer stays fresh. The value the home picker's
/// blocking cache used before any of this was routed, kept so a local
/// workspace's dot updates on exactly the cadence it always did.
const LOCAL_TTL: Duration = Duration::from_millis(2_000);

/// How long another machine's answer stays fresh. Longer than the local one
/// because the query is a routed round trip rather than a unix socket, and a
/// dot is not worth a heartbeat.
const REMOTE_TTL: Duration = Duration::from_secs(10);

/// How long to sit on a *failed* answer before asking again.
///
/// Shorter than the success TTL, which looks backwards until you notice the two
/// are decaying different things. A landed answer is a fact, and facts about
/// which shells are running go stale slowly. A failure is the absence of a fact:
/// it carries nothing, it is the state the user most wants corrected, and the
/// thing that would correct it — the link coming back — is exactly what this
/// interval decides how fast we notice.
const UNREACHABLE_TTL: Duration = Duration::from_secs(6);

/// How often [`sweep`] is allowed to walk the workspace list.
///
/// The sweep itself is called from `render`, which on a 120Hz display is 120
/// times a second; the walk builds a `pane_ids()` vector and a connection key
/// per workspace, which is not free enough to do that often. Everything past
/// this gate is idempotent, so the only cost of the gate is that a probe may
/// start up to a quarter-second late.
const SWEEP_INTERVAL: Duration = Duration::from_millis(250);

thread_local! {
    /// When [`sweep`] last walked the list.
    ///
    /// Deliberately *not* a field of [`PaneLivenessCache`]: reaching a global
    /// mutably notifies its observers, the app repaints on that notification,
    /// and the repaint sweeps again — a stamp stored in the global would spin
    /// the render loop at full speed forever. It is also honestly thread-local
    /// state, since only the UI thread ever sweeps.
    static LAST_SWEEP: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// What is known about a workspace's panes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Liveness {
    /// The machine answered and at least one of the workspace's panes is still
    /// running: reopening it reattaches to live shells.
    Alive,
    /// The machine answered and none of them are: the saved layout is all that
    /// is left, and reopening spawns fresh.
    Stopped,
    /// The machine could not be asked. Not the same as `Stopped` — see the
    /// module docs.
    Unknown,
}

/// One machine's last landed answer.
struct Answer {
    /// When it landed, for the TTL.
    at: Instant,
    /// The live pane ids the daemon reported, or `None` when the query failed.
    alive: Option<HashSet<u64>>,
}

impl Answer {
    /// Whether this answer may still be used without re-asking.
    fn fresh(&self, host: HostId) -> bool {
        let ttl = match (&self.alive, host.is_local()) {
            (None, _) => UNREACHABLE_TTL,
            (Some(_), true) => LOCAL_TTL,
            (Some(_), false) => REMOTE_TTL,
        };
        self.at.elapsed() < ttl
    }
}

/// The process-wide liveness store (a gpui [`Global`](gpui::Global)), keyed by
/// machine.
///
/// Per **machine**, not per workspace: two workspaces on one box share a pane
/// registry, so they share one query. That is the same granularity
/// [`HostId`] already has everywhere else in the client.
#[derive(Default)]
pub struct PaneLivenessCache {
    answers: HashMap<HostId, Answer>,
    /// Queries out, so a render that re-asks before one lands does not start a
    /// second. There is nothing to supersede a liveness answer with, so only
    /// the in-flight half of [`InFlight`] is used.
    probes: InFlight<HostId>,
}

impl gpui::Global for PaneLivenessCache {}

impl PaneLivenessCache {
    /// Whether any of `pane_ids` is still running on `host`.
    ///
    /// `pane_ids` must be the ids **that machine** minted — i.e. the ids of a
    /// workspace whose `host_id()` is `host`. Mixing them is the bug the whole
    /// module exists to prevent, and keying by host is how it is prevented:
    /// there is no way to spell the question without naming the machine.
    pub fn liveness(&self, host: HostId, pane_ids: &[u64]) -> Liveness {
        // Claims nothing, so nothing about it is in question — not even on a
        // machine we cannot reach. Answered before the cache is consulted so an
        // empty workspace never draws the "unknown" dot while it waits for an
        // answer that could not change it.
        if pane_ids.is_empty() {
            return Liveness::Stopped;
        }
        match self.alive_set(host) {
            Some(alive) => {
                if pane_ids.iter().any(|id| alive.contains(id)) {
                    Liveness::Alive
                } else {
                    Liveness::Stopped
                }
            }
            // Never asked, or asked and refused. On this machine that is not a
            // mystery — an unreachable local daemon is a daemon with no panes
            // in it — so local resolves to `Stopped` and keeps the pre-remote
            // rendering exactly as it was.
            None if host.is_local() => Liveness::Stopped,
            None => Liveness::Unknown,
        }
    }

    /// The live ids `host` last reported, or `None` when the last word from it
    /// was a failure (or there has been no word at all).
    fn alive_set(&self, host: HostId) -> Option<&HashSet<u64>> {
        self.answers.get(&host)?.alive.as_ref()
    }

    /// Whether a probe for `host` is worth starting: nothing fresh cached, and
    /// nothing already in flight.
    pub fn needs_probe(&self, host: HostId) -> bool {
        !self.probes.is_pending(&host)
            && !self
                .answers
                .get(&host)
                .is_some_and(|answer| answer.fresh(host))
    }

    /// Claim the probe for `host`. `false` when someone else already has it.
    pub fn begin_probe(&mut self, host: HostId) -> bool {
        self.probes.begin(host)
    }

    /// Fold a landed probe in: `Some(ids)` when the daemon answered, `None`
    /// when it could not be reached.
    pub fn finish_probe(&mut self, host: HostId, alive: Option<HashSet<u64>>) {
        self.probes.finish(&host);
        self.answers.insert(
            host,
            Answer {
                at: Instant::now(),
                alive,
            },
        );
    }

    /// Forget what `host` said, so the next sweep asks again.
    ///
    /// For the moments the app itself made the answer wrong — stopping a
    /// workspace kills panes the cache still lists — where waiting out the TTL
    /// would leave a green dot on a workspace the user just shut down.
    pub fn invalidate(&mut self, host: HostId) {
        self.answers.remove(&host);
    }
}

/// [`PaneLivenessCache::liveness`] for a whole workspace, read-only.
///
/// The one call the render sites make. It cannot ask the wrong machine: the
/// host and the ids both come off the same [`WindowView`].
pub fn liveness_of(cx: &App, workspace: &WindowView) -> Liveness {
    let host = workspace.host_id();
    // The ids live in the machine's tree; its mirror is where they are read. A
    // machine whose tree has not been pulled leaves us with no question to ask,
    // which is `Unknown` on any machine — reading it as `Stopped` would tell the
    // user their sessions are gone on the strength of our own ignorance. See the
    // module docs for why this is *not* the same as a failed local `List`.
    let Some(ids) = crate::ui::machine_mirror::pane_ids(cx, workspace) else {
        return Liveness::Unknown;
    };
    match cx.try_global::<PaneLivenessCache>() {
        Some(cache) => cache.liveness(host, &ids),
        // Before the app has installed the global. Asked of an empty cache
        // rather than answered here, so there is exactly one place that decides
        // what "nothing known yet" looks like and the first frame draws what
        // every later one will.
        None => PaneLivenessCache::default().liveness(host, &ids),
    }
}

/// Start whatever liveness queries the workspace list is missing.
///
/// Safe to call from `render`: it reads the cache, may start background work,
/// and never blocks or waits. Rate-limited by [`SWEEP_INTERVAL`], deduplicated
/// per machine by [`InFlight`], and gated per machine by the TTL — so a picker
/// sitting open does not turn into a query loop.
pub fn sweep(cx: &mut App) {
    let now = Instant::now();
    if LAST_SWEEP.get().is_some_and(|at| now < at + SWEEP_INTERVAL) {
        return;
    }
    LAST_SWEEP.set(Some(now));

    // One workspace per machine is enough to build that machine's route, and
    // only workspaces that claim panes have anything to ask about. Collected
    // first so the borrow of the store is released before the probes, which
    // need `cx` mutably.
    let mut targets: Vec<(HostId, WorkspaceId)> = Vec::new();
    for w in &WorkspaceStore::all(cx).views {
        let host = w.host_id();
        if targets.iter().any(|(seen, _)| *seen == host) {
            continue;
        }
        if crate::ui::machine_mirror::pane_ids(cx, w).is_none_or(|ids| ids.is_empty()) {
            continue;
        }
        targets.push((host, w.id));
    }
    for (host, workspace) in targets {
        probe_host(cx, host, workspace);
    }
}

/// Ask one machine, in the background.
///
/// `workspace` is only used to build the route; the answer is stored against
/// the machine, and every workspace on it reads the same one.
fn probe_host(cx: &mut App, host: HostId, workspace: WorkspaceId) {
    if !cx
        .try_global::<PaneLivenessCache>()
        .is_some_and(|cache| cache.needs_probe(host))
    {
        return;
    }
    // Never dial for a dot. A routed query to a machine this process is not
    // connected to would have the daemon open an SSH session — with whatever
    // passphrase prompt and multi-second handshake that entails — because a
    // picker row was on screen. Unconnected stays `Unknown`, which is exactly
    // what it is.
    //
    // Recorded as a landed failure rather than returned from: a bare `return`
    // would leave `needs_probe` true, so the next frame would re-decide this,
    // and `HostLinks::get` reaches its global mutably — which notifies,
    // which repaints, which sweeps. Storing the answer puts the decision behind
    // the same TTL as every other one.
    if !host.is_local() && crate::ui::remote_connect::HostLinks::get(cx, host).is_none() {
        cx.update_global::<PaneLivenessCache, _>(|cache, _| cache.finish_probe(host, None));
        return;
    }
    let route = crate::ui::remote_workspace::pane_route_for(cx, workspace);
    // `global_mut` notifies, so the claim is taken last: everything above can
    // decline without costing a repaint.
    if !cx.global_mut::<PaneLivenessCache>().begin_probe(host) {
        return;
    }
    cx.spawn(async move |cx| {
        let alive = cx.background_spawn(async move { query(&route) }).await;
        // Landed through `update_global`, which is what wakes the
        // `observe_global` hook that repaints the picker and the title bar.
        cx.update(|cx| {
            cx.update_global::<PaneLivenessCache, _>(|cache, _| cache.finish_probe(host, alive));
        });
    })
    .detach();
}

/// The blocking half: one `List` down `route`, reduced to the ids that are
/// still running. `None` is "could not ask", which is the distinction the whole
/// three-state rendering rests on.
fn query(route: &PaneRoute) -> Option<HashSet<u64>> {
    match RemoteTerminal::try_list_panes_on(route) {
        Ok(panes) => Some(
            panes
                .into_iter()
                .filter(|p| p.alive)
                .map(|p| p.pane_id)
                .collect(),
        ),
        Err(e) => {
            log::debug!("pane liveness query failed: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A remote machine, and a second one, so "keyed by host" can be tested
    /// rather than asserted.
    fn box_a() -> HostId {
        HostId::from_connection_key("ssh-direct:me@a:22")
    }
    fn box_b() -> HostId {
        HostId::from_connection_key("ssh-direct:me@b:22")
    }

    /// The three states, each from the input that produces it.
    #[test]
    fn the_three_states_come_from_three_different_situations() {
        let mut cache = PaneLivenessCache::default();
        let host = box_a();

        // Never asked: not "stopped" — unknown.
        assert_eq!(cache.liveness(host, &[1, 2]), Liveness::Unknown);

        // Asked, and the machine listed one of them.
        cache.finish_probe(host, Some(HashSet::from([2, 9])));
        assert_eq!(cache.liveness(host, &[1, 2]), Liveness::Alive);

        // Asked, and none of this workspace's panes are in the answer.
        assert_eq!(cache.liveness(host, &[1, 3]), Liveness::Stopped);

        // Asked and could not be reached: back to unknown, *not* stopped —
        // the panes are very probably still running over there.
        cache.finish_probe(host, None);
        assert_eq!(cache.liveness(host, &[1, 2]), Liveness::Unknown);
    }

    /// The bug. Two machines mint the same small pane ids, and a workspace on
    /// one must never be lit up by the other's registry.
    #[test]
    fn one_machines_answer_never_speaks_for_another() {
        let mut cache = PaneLivenessCache::default();
        // The local daemon is running panes 1 and 2 — the ids a fresh daemon
        // on any machine hands out first.
        cache.finish_probe(HostId::LOCAL, Some(HashSet::from([1, 2])));
        // The box has been off for a week: nothing of its own is claimed here.
        assert_eq!(cache.liveness(box_a(), &[1, 2]), Liveness::Unknown);
        // And a *third* machine's answer does not leak into the second's.
        cache.finish_probe(box_b(), Some(HashSet::from([1, 2])));
        assert_eq!(cache.liveness(box_a(), &[1, 2]), Liveness::Unknown);
        assert_eq!(cache.liveness(box_b(), &[1, 2]), Liveness::Alive);
        // Local still reads exactly as it always did.
        assert_eq!(cache.liveness(HostId::LOCAL, &[1, 2]), Liveness::Alive);
        assert_eq!(cache.liveness(HostId::LOCAL, &[7]), Liveness::Stopped);
    }

    /// This machine never shows "unknown": an unreachable local daemon is a
    /// daemon with nothing running in it, and the picker drew that as a plain
    /// badge long before any of this was routed.
    #[test]
    fn local_never_renders_as_unknown() {
        let mut cache = PaneLivenessCache::default();
        // Nothing asked yet — the state every first frame is in.
        assert_eq!(cache.liveness(HostId::LOCAL, &[1]), Liveness::Stopped);
        // Asked, and no daemon answered.
        cache.finish_probe(HostId::LOCAL, None);
        assert_eq!(cache.liveness(HostId::LOCAL, &[1]), Liveness::Stopped);
    }

    /// A workspace that claims no panes is stopped on any machine — there is
    /// nothing for an answer to contain, so it is not "unknown" either, even on
    /// a machine that was never asked. Caught in the GUI: an empty remote
    /// workspace sat in the picker wearing the muted dot, which reads as "we
    /// could not check" about a workspace there is nothing to check.
    #[test]
    fn a_workspace_with_no_claimed_panes_is_never_alive() {
        let mut cache = PaneLivenessCache::default();
        assert_eq!(cache.liveness(box_a(), &[]), Liveness::Stopped);
        assert_eq!(cache.liveness(HostId::LOCAL, &[]), Liveness::Stopped);
        cache.finish_probe(box_a(), Some(HashSet::from([1, 2, 3])));
        assert_eq!(cache.liveness(box_a(), &[]), Liveness::Stopped);
        cache.finish_probe(box_a(), None);
        assert_eq!(cache.liveness(box_a(), &[]), Liveness::Stopped);
    }

    /// One query per machine in flight, and a fresh answer stops the asking —
    /// this is what keeps a picker on screen from becoming a query loop.
    #[test]
    fn probes_are_deduplicated_and_then_throttled_by_the_ttl() {
        let mut cache = PaneLivenessCache::default();
        let host = box_a();

        assert!(cache.needs_probe(host), "nothing cached: ask");
        assert!(cache.begin_probe(host));
        assert!(!cache.needs_probe(host), "one is already out");
        assert!(!cache.begin_probe(host), "and it cannot be claimed twice");

        cache.finish_probe(host, Some(HashSet::from([1])));
        assert!(!cache.needs_probe(host), "the answer is fresh");

        // A failure is cached too, or an unreachable machine would be retried
        // on every frame — each retry a connect timeout on a background task.
        cache.finish_probe(host, None);
        assert!(!cache.needs_probe(host));

        // Invalidation is the way back to asking, for the moments the app
        // itself made the answer wrong.
        cache.invalidate(host);
        assert!(cache.needs_probe(host));
    }

    /// Each machine's freshness is its own: a fresh local answer must not stop
    /// the sweep from asking the box.
    #[test]
    fn freshness_is_per_machine() {
        let mut cache = PaneLivenessCache::default();
        cache.finish_probe(HostId::LOCAL, Some(HashSet::new()));
        assert!(!cache.needs_probe(HostId::LOCAL));
        assert!(cache.needs_probe(box_a()));
    }

    /// The TTLs are ordered the way the comments claim: a local socket may be
    /// re-asked far sooner than a routed round trip, and a failure — which is
    /// not a fact and is the state the user most wants corrected — is retried
    /// sooner than a success is refreshed.
    #[test]
    fn ttls_are_ordered_by_what_the_query_costs() {
        assert!(LOCAL_TTL < UNREACHABLE_TTL);
        assert!(UNREACHABLE_TTL < REMOTE_TTL);
        assert!(SWEEP_INTERVAL < LOCAL_TTL);
    }
}

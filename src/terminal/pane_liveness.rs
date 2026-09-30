use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, BorrowAppContext as _};

use crate::core::session::{WindowView, WorkspaceId, WorkspaceStore};
use crate::terminal::{PaneRoute, RemoteTerminal};
use crate::ui::host_ops::{HostId, InFlight};

const LOCAL_TTL: Duration = Duration::from_millis(2_000);

const REMOTE_TTL: Duration = Duration::from_secs(10);

const UNREACHABLE_TTL: Duration = Duration::from_secs(6);

const SWEEP_INTERVAL: Duration = Duration::from_millis(250);

thread_local! {
                                static LAST_SWEEP: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Liveness {
    Alive,
    Stopped,
    Unknown,
}

struct Answer {
    at: Instant,
    alive: Option<HashSet<u64>>,
}

impl Answer {
    fn fresh(&self, host: HostId) -> bool {
        let ttl = match (&self.alive, host.is_local()) {
            (None, _) => UNREACHABLE_TTL,
            (Some(_), true) => LOCAL_TTL,
            (Some(_), false) => REMOTE_TTL,
        };
        self.at.elapsed() < ttl
    }
}

#[derive(Default)]
pub struct PaneLivenessCache {
    answers: HashMap<HostId, Answer>,
    probes: InFlight<HostId>,
}

impl gpui::Global for PaneLivenessCache {}

impl PaneLivenessCache {
    pub fn liveness(&self, host: HostId, pane_ids: &[u64]) -> Liveness {
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
            None if host.is_local() => Liveness::Stopped,
            None => Liveness::Unknown,
        }
    }

    fn alive_set(&self, host: HostId) -> Option<&HashSet<u64>> {
        self.answers.get(&host)?.alive.as_ref()
    }

    pub fn needs_probe(&self, host: HostId) -> bool {
        !self.probes.is_pending(&host)
            && !self
                .answers
                .get(&host)
                .is_some_and(|answer| answer.fresh(host))
    }

    pub fn begin_probe(&mut self, host: HostId) -> bool {
        self.probes.begin(host)
    }

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

    pub fn invalidate(&mut self, host: HostId) {
        self.answers.remove(&host);
    }
}

pub fn liveness_of(cx: &App, workspace: &WindowView) -> Liveness {
    let host = workspace.host_id();
    let Some(ids) = crate::ui::machine_mirror::pane_ids(cx, workspace) else {
        return Liveness::Unknown;
    };
    match cx.try_global::<PaneLivenessCache>() {
        Some(cache) => cache.liveness(host, &ids),
        None => PaneLivenessCache::default().liveness(host, &ids),
    }
}

pub fn sweep(cx: &mut App) {
    let now = Instant::now();
    if LAST_SWEEP.get().is_some_and(|at| now < at + SWEEP_INTERVAL) {
        return;
    }
    LAST_SWEEP.set(Some(now));

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

fn probe_host(cx: &mut App, host: HostId, workspace: WorkspaceId) {
    if !cx
        .try_global::<PaneLivenessCache>()
        .is_some_and(|cache| cache.needs_probe(host))
    {
        return;
    }
    // A remote machine is asked over the control link it already has. Asking
    // on a pane route instead opened an SSH session channel every ten seconds
    // — one of the few the server allows per connection, and once those were
    // all taken by panes, a whole new connection with its handshake each time.
    let probe = if host.is_local() {
        Probe::Route(crate::ui::remote_workspace::pane_route_for(cx, workspace))
    } else {
        match crate::ui::remote_connect::HostLinks::get(cx, host) {
            Some(link) => Probe::Control(std::sync::Arc::clone(link.client())),
            None => {
                cx.update_global::<PaneLivenessCache, _>(|cache, _| cache.finish_probe(host, None));
                return;
            }
        }
    };
    if !cx.global_mut::<PaneLivenessCache>().begin_probe(host) {
        return;
    }
    cx.spawn(async move |cx| {
        let alive = cx.background_spawn(async move { probe.query() }).await;
        cx.update(|cx| {
            cx.update_global::<PaneLivenessCache, _>(|cache, _| cache.finish_probe(host, alive));
        });
    })
    .detach();
}

enum Probe {
    Route(PaneRoute),
    Control(std::sync::Arc<tty7_core::daemon::control::ControlClient>),
}

impl Probe {
    fn query(&self) -> Option<HashSet<u64>> {
        match self {
            Probe::Route(route) => query(route),
            Probe::Control(client) => query_control(client),
        }
    }
}

/// The machine tree marks each pane record with whether its daemon is running
/// it — overlaid from the daemon's own pane list, and cleared on load — which
/// is the same answer the pane socket's `List` gives.
fn query_control(client: &tty7_core::daemon::control::ControlClient) -> Option<HashSet<u64>> {
    use tty7_core::daemon::control::{ControlRequest, ReplyOk};
    match client.call(ControlRequest::MachineGet) {
        Ok(ReplyOk::MachineTree(machine)) => Some(live_panes(&machine)),
        Ok(other) => {
            log::debug!("pane liveness query got an unexpected reply: {other:?}");
            None
        }
        Err(e) => {
            log::debug!("pane liveness query failed: {e}");
            None
        }
    }
}

fn live_panes(machine: &crate::core::machine::Machine) -> HashSet<u64> {
    machine
        .panes
        .iter()
        .filter(|p| p.live)
        .map(|p| p.id)
        .collect()
}

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

    fn box_a() -> HostId {
        HostId::from_connection_key("ssh-direct:me@a:22")
    }
    fn box_b() -> HostId {
        HostId::from_connection_key("ssh-direct:me@b:22")
    }

    #[test]
    fn a_machine_tree_answers_with_the_panes_its_daemon_is_running() {
        let mut running = crate::core::machine::PaneRecord::new(3);
        running.live = true;
        let exited = crate::core::machine::PaneRecord::new(4);
        let machine = crate::core::machine::Machine {
            workspaces: Vec::new(),
            panes: vec![running, exited],
        };
        assert_eq!(live_panes(&machine), HashSet::from([3]));
    }

    #[test]
    fn the_three_states_come_from_three_different_situations() {
        let mut cache = PaneLivenessCache::default();
        let host = box_a();

        assert_eq!(cache.liveness(host, &[1, 2]), Liveness::Unknown);

        cache.finish_probe(host, Some(HashSet::from([2, 9])));
        assert_eq!(cache.liveness(host, &[1, 2]), Liveness::Alive);

        assert_eq!(cache.liveness(host, &[1, 3]), Liveness::Stopped);

        cache.finish_probe(host, None);
        assert_eq!(cache.liveness(host, &[1, 2]), Liveness::Unknown);
    }

    #[test]
    fn one_machines_answer_never_speaks_for_another() {
        let mut cache = PaneLivenessCache::default();
        cache.finish_probe(HostId::LOCAL, Some(HashSet::from([1, 2])));
        assert_eq!(cache.liveness(box_a(), &[1, 2]), Liveness::Unknown);
        cache.finish_probe(box_b(), Some(HashSet::from([1, 2])));
        assert_eq!(cache.liveness(box_a(), &[1, 2]), Liveness::Unknown);
        assert_eq!(cache.liveness(box_b(), &[1, 2]), Liveness::Alive);
        assert_eq!(cache.liveness(HostId::LOCAL, &[1, 2]), Liveness::Alive);
        assert_eq!(cache.liveness(HostId::LOCAL, &[7]), Liveness::Stopped);
    }

    #[test]
    fn local_never_renders_as_unknown() {
        let mut cache = PaneLivenessCache::default();
        assert_eq!(cache.liveness(HostId::LOCAL, &[1]), Liveness::Stopped);
        cache.finish_probe(HostId::LOCAL, None);
        assert_eq!(cache.liveness(HostId::LOCAL, &[1]), Liveness::Stopped);
    }

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

        cache.finish_probe(host, None);
        assert!(!cache.needs_probe(host));

        cache.invalidate(host);
        assert!(cache.needs_probe(host));
    }

    #[test]
    fn freshness_is_per_machine() {
        let mut cache = PaneLivenessCache::default();
        cache.finish_probe(HostId::LOCAL, Some(HashSet::new()));
        assert!(!cache.needs_probe(HostId::LOCAL));
        assert!(cache.needs_probe(box_a()));
    }

    #[test]
    fn ttls_are_ordered_by_what_the_query_costs() {
        assert!(LOCAL_TTL < UNREACHABLE_TTL);
        assert!(UNREACHABLE_TTL < REMOTE_TTL);
        assert!(SWEEP_INTERVAL < LOCAL_TTL);
    }
}

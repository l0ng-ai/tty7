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
    if !host.is_local() && crate::ui::remote_connect::HostLinks::get(cx, host).is_none() {
        cx.update_global::<PaneLivenessCache, _>(|cache, _| cache.finish_probe(host, None));
        return;
    }
    let route = crate::ui::remote_workspace::pane_route_for(cx, workspace);
    if !cx.global_mut::<PaneLivenessCache>().begin_probe(host) {
        return;
    }
    cx.spawn(async move |cx| {
        let alive = cx.background_spawn(async move { query(&route) }).await;
        cx.update(|cx| {
            cx.update_global::<PaneLivenessCache, _>(|cache, _| cache.finish_probe(host, alive));
        });
    })
    .detach();
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

//! CPU% and memory for Info → Processes.
//!
//! The daemon sends raw CPU time; the percentage needs two samples of the same
//! process, so it is worked out here, where the panel's poll keeps state
//! between rounds and the protocol stays stateless.

use std::collections::HashMap;
use std::time::Instant;

use gpui::{App, IntoElement, ParentElement as _, SharedString, Styled as _, div, px, rems};
use gpui_component::{ActiveTheme as _, h_flex};

use crate::daemon::procstat::compact_bytes;
use crate::daemon::protocol::ProcEntry;
use crate::ui::i18n::{L10nKey, t};
use crate::ui::right_panel::{META, META_MONO};

const CPU_W: f32 = 40.;
const MEM_W: f32 = 52.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sample {
    started: Option<u64>,
    cpu_ns: u64,
}

#[derive(Default)]
pub(crate) struct CpuTracker {
    prev: HashMap<u32, Sample>,
    at: Option<Instant>,
    pct: HashMap<u32, f64>,
}

impl CpuTracker {
    pub(crate) fn sample(&mut self, procs: &[ProcEntry], now: Instant) {
        let elapsed_ns = self
            .at
            .map_or(0, |at| now.saturating_duration_since(at).as_nanos() as u64);
        let mut next = HashMap::new();
        self.pct.clear();
        for p in procs {
            let Some(cpu_ns) = p.cpu_ns else { continue };
            let cur = Sample {
                started: p.started,
                cpu_ns,
            };
            if let Some(pct) = cpu_percent(self.prev.get(&p.pid).copied(), cur, elapsed_ns) {
                self.pct.insert(p.pid, pct);
            }
            next.insert(p.pid, cur);
        }
        self.prev = next;
        self.at = Some(now);
    }

    pub(crate) fn percent(&self, pid: u32) -> Option<f64> {
        self.pct.get(&pid).copied()
    }
}

/// `ps`'s %CPU over the interval: 100 is one core busy the whole time. `None`
/// for a first sample, a pid now naming a different process, a clock that did
/// not move, or a CPU time that went backwards.
fn cpu_percent(prev: Option<Sample>, cur: Sample, elapsed_ns: u64) -> Option<f64> {
    let prev = prev?;
    if elapsed_ns == 0 || prev.started != cur.started || cur.cpu_ns < prev.cpu_ns {
        return None;
    }
    Some((cur.cpu_ns - prev.cpu_ns) as f64 * 100.0 / elapsed_ns as f64)
}

/// Summed CPU% and RSS over the rows that report them; `None` when none do.
pub(crate) fn totals(procs: &[ProcEntry], cpu: &CpuTracker) -> (Option<f64>, Option<u64>) {
    let pcts: Vec<f64> = procs.iter().filter_map(|p| cpu.percent(p.pid)).collect();
    let rss: Vec<u64> = procs.iter().filter_map(|p| p.rss).collect();
    (
        (!pcts.is_empty()).then(|| pcts.iter().sum()),
        (!rss.is_empty()).then(|| rss.iter().sum()),
    )
}

fn format_cpu(pct: f64) -> String {
    match pct < 10.0 {
        true => format!("{pct:.1}%"),
        false => format!("{pct:.0}%"),
    }
}

/// The CPU cell: blank for a daemon that sends no CPU time, a dash for a
/// process seen once so far.
fn cpu_text(reported: bool, pct: Option<f64>) -> String {
    match (reported, pct) {
        (_, Some(pct)) => format_cpu(pct),
        (true, None) => "–".into(),
        (false, None) => String::new(),
    }
}

fn cell(text: String, w: f32, mono: SharedString, cx: &App) -> impl IntoElement {
    div()
        .flex_none()
        .w(px(w))
        .flex()
        .justify_end()
        .text_size(rems(META_MONO))
        .font_family(mono)
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

/// CPU% and memory, right-aligned in fixed columns so rows line up.
pub(crate) fn row_cells(
    p: &ProcEntry,
    cpu: &CpuTracker,
    mono: SharedString,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .flex_none()
        .gap(px(6.))
        .child(cell(
            cpu_text(p.cpu_ns.is_some(), cpu.percent(p.pid)),
            CPU_W,
            mono.clone(),
            cx,
        ))
        .child(cell(
            p.rss.map(compact_bytes).unwrap_or_default(),
            MEM_W,
            mono,
            cx,
        ))
}

/// The Total line under the tree, or nothing from a daemon that reports no
/// usage at all.
pub(crate) fn total_row(
    procs: &[ProcEntry],
    cpu: &CpuTracker,
    mono: SharedString,
    cx: &App,
) -> Option<gpui::Div> {
    let reported = procs.iter().any(|p| p.cpu_ns.is_some());
    let (pct, rss) = totals(procs, cpu);
    if !reported && rss.is_none() {
        return None;
    }
    Some(
        h_flex()
            .h(px(super::right_panel::PROC_ROW_H))
            .px(px(super::right_panel::ROW_INSET))
            .items_center()
            .gap(px(6.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(rems(META))
                    .text_color(cx.theme().muted_foreground)
                    .child(t(L10nKey::PanelProcessesTotal).to_string()),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(6.))
                    .child(cell(cpu_text(reported, pct), CPU_W, mono.clone(), cx))
                    .child(cell(
                        rss.map(compact_bytes).unwrap_or_default(),
                        MEM_W,
                        mono,
                        cx,
                    )),
            ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn s(started: u64, cpu_ns: u64) -> Sample {
        Sample {
            started: Some(started),
            cpu_ns,
        }
    }

    #[test]
    fn cpu_percent_is_ps_style() {
        assert_eq!(
            cpu_percent(Some(s(1, 0)), s(1, 500_000_000), 1_000_000_000),
            Some(50.0)
        );
        assert_eq!(
            cpu_percent(Some(s(1, 0)), s(1, 2_000_000_000), 1_000_000_000),
            Some(200.0)
        );
    }

    #[test]
    fn no_percent_without_a_fair_comparison() {
        assert_eq!(cpu_percent(None, s(1, 10), 1_000), None, "first sample");
        assert_eq!(
            cpu_percent(Some(s(1, 0)), s(2, 10), 1_000),
            None,
            "pid reused"
        );
        assert_eq!(
            cpu_percent(Some(s(1, 0)), s(1, 10), 0),
            None,
            "zero interval"
        );
        assert_eq!(
            cpu_percent(Some(s(1, 50)), s(1, 10), 1_000),
            None,
            "went backwards"
        );
    }

    fn proc(pid: u32, rss: Option<u64>, cpu_ns: Option<u64>) -> ProcEntry {
        ProcEntry {
            pid,
            rss,
            cpu_ns,
            started: Some(7),
            ..Default::default()
        }
    }

    #[test]
    fn totals_sum_what_was_reported() {
        let t0 = Instant::now();
        let mut cpu = CpuTracker::default();
        cpu.sample(&[proc(1, None, Some(0)), proc(2, None, Some(0))], t0);
        assert_eq!(cpu.percent(1), None, "first sample shows a dash");
        let procs = [
            proc(1, Some(1024), Some(250_000_000)),
            proc(2, Some(2048), Some(500_000_000)),
            proc(3, None, None),
        ];
        cpu.sample(&procs, t0 + Duration::from_secs(1));
        assert_eq!(totals(&procs, &cpu), (Some(75.0), Some(3072)));
        assert_eq!(totals(&[proc(3, None, None)], &cpu), (None, None));
    }

    #[test]
    fn cpu_cells_read_blank_dash_or_percent() {
        assert_eq!(cpu_text(false, None), "");
        assert_eq!(cpu_text(true, None), "–");
        assert_eq!(cpu_text(true, Some(12.4)), "12%");
        assert_eq!(cpu_text(true, Some(0.34)), "0.3%");
    }
}

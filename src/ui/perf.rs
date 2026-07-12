//! Optional, label-keyed build-time profiling for the UI layer. Disabled unless
//! `TTY7_PROFILE` is set to a non-empty, non-`0` value (e.g.
//! `TTY7_PROFILE=1 cargo run`).
//!
//! Where [`crate::terminal::fps`] measures the *paint* cost of the terminal grid,
//! this measures how long a GPUI view spends *building* its element tree in
//! `render`, and — just as usefully — how often that `render` runs. A view whose
//! build is cheap but fires dozens of times a second (a runaway `cx.notify()`
//! loop) reads as high "calls/s" here even when each call is fast, which is
//! exactly the signal needed to tell "one expensive rebuild" apart from "a cheap
//! rebuild in a tight loop".
//!
//! It reports the CPU-side build cost only (assembling the element tree and
//! returning it); GPU paint is out of scope — pair with `TTY7_FPS` or
//! Instruments for the paint side.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Whether profiling is on. Read once from `TTY7_PROFILE` and cached.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| flag_enables(std::env::var("TTY7_PROFILE").ok().as_deref()))
}

/// Whether a `TTY7_PROFILE` value (or its absence) turns profiling on: any
/// non-empty value except `0`. Split out so the semantics are testable without
/// depending on the ambient process environment.
fn flag_enables(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// One aggregation window of wall-clock time *in which building happened* — an
/// idle gap just stretches the reported window rather than reading as a low rate.
const WINDOW: Duration = Duration::from_secs(1);

struct Meter {
    window_start: Instant,
    calls: u32,
    total: Duration,
    max: Duration,
}

impl Meter {
    fn new(window_start: Instant) -> Self {
        Self {
            window_start,
            calls: 0,
            total: Duration::ZERO,
            max: Duration::ZERO,
        }
    }

    /// Fold one build in; when `now` crosses the window boundary, return the
    /// aggregate report line and start a fresh window anchored at `now`. The
    /// clock is injected so tests can cross windows without sleeping.
    fn record(&mut self, label: &str, now: Instant, build: Duration) -> Option<String> {
        self.calls += 1;
        self.total += build;
        self.max = self.max.max(build);

        let elapsed = now.duration_since(self.window_start);
        if elapsed < WINDOW {
            return None;
        }
        let secs = elapsed.as_secs_f64();
        let rate = self.calls as f64 / secs;
        let avg_ms = self.total.as_secs_f64() * 1000.0 / self.calls as f64;
        let max_ms = self.max.as_secs_f64() * 1000.0;
        let line = format!(
            "[perf] {label}: {rate:.1} calls/s over {secs:.2}s ({} calls) | build avg {avg_ms:.2}ms max {max_ms:.2}ms",
            self.calls
        );
        *self = Meter::new(now);
        Some(line)
    }
}

fn meters() -> &'static Mutex<HashMap<&'static str, Meter>> {
    static M: OnceLock<Mutex<HashMap<&'static str, Meter>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record one build's CPU-side duration under `label`. Emits an aggregate stderr
/// line per `label` roughly once per `WINDOW` of building time. No-op unless
/// [`enabled`]; callers still gate the surrounding `Instant::now()` on `enabled`
/// so a normal run pays nothing.
pub fn record(label: &'static str, build: Duration) {
    let now = Instant::now();
    let mut guard = meters().lock().unwrap();
    let m = guard.entry(label).or_insert_with(|| Meter::new(now));
    if let Some(line) = m.record(label, now, build) {
        // Direct to stderr: the app never initialises a `log` backend, so
        // `log::info!` here would be silently dropped.
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_semantics_cover_unset_empty_zero_and_set() {
        assert!(!flag_enables(None), "unset leaves profiling off");
        assert!(!flag_enables(Some("")), "empty value is off");
        assert!(!flag_enables(Some("0")), "explicit 0 is off");
        assert!(flag_enables(Some("1")));
        assert!(flag_enables(Some("yes")));
    }

    #[test]
    fn meter_accumulates_silently_below_the_window() {
        let start = Instant::now();
        let mut m = Meter::new(start);
        assert_eq!(
            m.record(
                "x",
                start + Duration::from_millis(10),
                Duration::from_millis(2)
            ),
            None
        );
        assert_eq!(
            m.calls, 1,
            "the sub-window build folded into the open window"
        );
    }

    #[test]
    fn meter_flushes_and_resets_after_a_window() {
        let start = Instant::now();
        let mut m = Meter::new(start);
        assert!(
            m.record(
                "render",
                start + Duration::from_millis(500),
                Duration::from_millis(2)
            )
            .is_none()
        );
        // Crossing the window boundary flushes the aggregate: 2 calls over 1.0s =
        // 2.0 calls/s, build avg (2+6)/2 = 4ms, max 6ms.
        let flush_at = start + Duration::from_millis(1000);
        let line = m
            .record("render", flush_at, Duration::from_millis(6))
            .expect("crossing the window emits the aggregate line");
        assert_eq!(
            line,
            "[perf] render: 2.0 calls/s over 1.00s (2 calls) | build avg 4.00ms max 6.00ms"
        );
        assert_eq!(m.calls, 0, "the flush starts a fresh window");
        assert_eq!(m.window_start, flush_at);
    }
}

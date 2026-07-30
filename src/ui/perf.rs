use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| flag_enables(std::env::var("TTY7_PROFILE").ok().as_deref()))
}

fn flag_enables(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

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

pub fn record(label: &'static str, build: Duration) {
    let now = Instant::now();
    let mut guard = meters().lock().unwrap();
    let m = guard.entry(label).or_insert_with(|| Meter::new(now));
    if let Some(line) = m.record(label, now, build) {
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

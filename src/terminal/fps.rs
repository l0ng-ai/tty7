use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| flag_enables(std::env::var("TTY7_FPS").ok().as_deref()))
}

fn flag_enables(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

const WINDOW: Duration = Duration::from_secs(1);

struct Meter {
    window_start: Instant,
    frames: u32,
    paint_total: Duration,
    paint_max: Duration,
}

impl Meter {
    fn new(window_start: Instant) -> Self {
        Self {
            window_start,
            frames: 0,
            paint_total: Duration::ZERO,
            paint_max: Duration::ZERO,
        }
    }

    fn record(&mut self, now: Instant, paint: Duration) -> Option<String> {
        self.frames += 1;
        self.paint_total += paint;
        self.paint_max = self.paint_max.max(paint);

        let elapsed = now.duration_since(self.window_start);
        if elapsed < WINDOW {
            return None;
        }
        let secs = elapsed.as_secs_f64();
        let fps = self.frames as f64 / secs;
        let avg_ms = self.paint_total.as_secs_f64() * 1000.0 / self.frames as f64;
        let max_ms = self.paint_max.as_secs_f64() * 1000.0;
        let line = format!(
            "[fps] {fps:.1} fps over {secs:.2}s ({} frames) | paint avg {avg_ms:.2}ms max {max_ms:.2}ms",
            self.frames
        );
        *self = Meter::new(now);
        Some(line)
    }
}

fn meter() -> &'static Mutex<Option<Meter>> {
    static M: OnceLock<Mutex<Option<Meter>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(None))
}

pub fn record(paint: Duration) {
    let now = Instant::now();
    let mut guard = meter().lock().unwrap();
    let m = guard.get_or_insert_with(|| Meter::new(now));
    if let Some(line) = m.record(now, paint) {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_semantics_cover_unset_empty_zero_and_set() {
        assert!(!flag_enables(None), "unset leaves timing off");
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
            m.record(start + Duration::from_millis(10), Duration::from_millis(2)),
            None
        );
        assert_eq!(
            m.record(start + Duration::from_millis(20), Duration::from_millis(5)),
            None
        );
        assert_eq!(m.frames, 2, "both frames folded into the open window");
    }

    #[test]
    fn meter_flushes_and_resets_after_a_window() {
        let start = Instant::now();
        let mut m = Meter::new(start);
        assert!(
            m.record(start + Duration::from_millis(100), Duration::from_millis(2))
                .is_none()
        );
        assert!(
            m.record(start + Duration::from_millis(200), Duration::from_millis(6))
                .is_none()
        );
        let flush_at = start + Duration::from_millis(1500);
        let line = m
            .record(flush_at, Duration::from_millis(4))
            .expect("crossing the window emits the aggregate line");
        assert_eq!(
            line,
            "[fps] 2.0 fps over 1.50s (3 frames) | paint avg 4.00ms max 6.00ms"
        );
        assert_eq!(m.frames, 0);
        assert_eq!(m.paint_total, Duration::ZERO);
        assert_eq!(m.paint_max, Duration::ZERO);
        assert_eq!(m.window_start, flush_at);
    }

    #[test]
    fn meter_flushes_exactly_on_the_window_boundary() {
        let start = Instant::now();
        let mut m = Meter::new(start);
        let line = m.record(start + WINDOW, Duration::from_millis(1));
        assert!(line.is_some(), "a frame exactly at the boundary flushes");
        assert!(line.unwrap().contains("(1 frames)"));
    }
}

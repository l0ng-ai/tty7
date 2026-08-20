//! A windowed rate-and-latency meter, shared by the two debug counters.
//!
//! `terminal::fps` and `ui::perf` each carried their own copy of this — same
//! window, same accumulate-then-flush shape, same environment-flag parsing —
//! differing only in the words on the line and whether the meter is keyed.
//! Those are the two things this parameterises.
//!
//! Nothing here runs unless the matching environment variable is set.

use std::time::{Duration, Instant};

/// How long samples accumulate before a line is emitted.
pub const WINDOW: Duration = Duration::from_secs(1);

/// Whether an environment variable's value turns a counter on.
///
/// Unset, empty and `0` are off; anything else is on. Taking the value rather
/// than reading the variable is what makes it testable.
pub fn flag_enables(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// The words that make a meter's line read as English.
///
/// One meter counts frames and times painting; the other counts calls and
/// times building. Everything else about them is identical.
pub struct Wording {
    /// Bracketed prefix identifying the counter, e.g. `fps`.
    pub tag: &'static str,
    /// Unit for the rate, e.g. `fps` or `calls/s`.
    pub rate_unit: &'static str,
    /// Plural noun for what was counted, e.g. `frames`.
    pub counted: &'static str,
    /// Verb for what was timed, e.g. `paint`.
    pub timed: &'static str,
}

pub struct Meter {
    window_start: Instant,
    count: u32,
    total: Duration,
    max: Duration,
}

impl Meter {
    pub fn new(window_start: Instant) -> Self {
        Self {
            window_start,
            count: 0,
            total: Duration::ZERO,
            max: Duration::ZERO,
        }
    }

    /// Fold one sample in, and return the summary line if the window closed.
    ///
    /// `label` names the thing being measured when a counter keeps one meter
    /// per callsite; the unkeyed counter passes `None` and gets no label.
    /// Emitting resets the meter, so a line covers exactly the window it
    /// reports.
    pub fn record(
        &mut self,
        now: Instant,
        sample: Duration,
        w: &Wording,
        label: Option<&str>,
    ) -> Option<String> {
        self.count += 1;
        self.total += sample;
        self.max = self.max.max(sample);

        let elapsed = now.duration_since(self.window_start);
        if elapsed < WINDOW {
            return None;
        }
        let secs = elapsed.as_secs_f64();
        let rate = self.count as f64 / secs;
        let avg_ms = self.total.as_secs_f64() * 1000.0 / self.count as f64;
        let max_ms = self.max.as_secs_f64() * 1000.0;
        let named = match label {
            Some(l) => format!(" {l}:"),
            None => String::new(),
        };
        let line = format!(
            "[{}]{named} {rate:.1} {} over {secs:.2}s ({} {}) | {} avg {avg_ms:.2}ms max {max_ms:.2}ms",
            w.tag, w.rate_unit, self.count, w.counted, w.timed
        );
        *self = Meter::new(now);
        Some(line)
    }

    /// Samples folded into the window still open. Tests read this; nothing else
    /// needs it.
    #[cfg(test)]
    pub fn count(&self) -> u32 {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: Wording = Wording {
        tag: "fps",
        rate_unit: "fps",
        counted: "frames",
        timed: "paint",
    };

    #[test]
    fn flag_semantics_cover_unset_empty_zero_and_set() {
        assert!(!flag_enables(None), "unset leaves the counter off");
        assert!(!flag_enables(Some("")), "empty value is off");
        assert!(!flag_enables(Some("0")), "explicit 0 is off");
        assert!(flag_enables(Some("1")));
        assert!(flag_enables(Some("yes")));
    }

    #[test]
    fn samples_below_the_window_accumulate_silently() {
        let start = Instant::now();
        let mut m = Meter::new(start);
        assert_eq!(
            m.record(
                start + Duration::from_millis(10),
                Duration::from_millis(2),
                &FPS,
                None
            ),
            None
        );
        assert_eq!(
            m.record(
                start + Duration::from_millis(20),
                Duration::from_millis(5),
                &FPS,
                None
            ),
            None
        );
        assert_eq!(m.count(), 2, "both samples folded into the open window");
    }

    #[test]
    fn crossing_the_window_flushes_and_resets() {
        let start = Instant::now();
        let mut m = Meter::new(start);
        assert!(
            m.record(
                start + Duration::from_millis(100),
                Duration::from_millis(2),
                &FPS,
                None
            )
            .is_none()
        );
        assert!(
            m.record(
                start + Duration::from_millis(200),
                Duration::from_millis(6),
                &FPS,
                None
            )
            .is_none()
        );
        let flush_at = start + Duration::from_millis(1500);
        let line = m
            .record(flush_at, Duration::from_millis(4), &FPS, None)
            .expect("crossing the window emits the aggregate line");
        assert_eq!(
            line,
            "[fps] 2.0 fps over 1.50s (3 frames) | paint avg 4.00ms max 6.00ms"
        );
        assert_eq!(m.count(), 0);
        assert_eq!(m.total, Duration::ZERO);
        assert_eq!(m.max, Duration::ZERO);
        assert_eq!(m.window_start, flush_at);
    }

    #[test]
    fn a_sample_exactly_on_the_boundary_flushes() {
        let start = Instant::now();
        let mut m = Meter::new(start);
        let line = m.record(start + WINDOW, Duration::from_millis(1), &FPS, None);
        assert!(line.is_some(), "a sample exactly at the boundary flushes");
        assert!(line.unwrap().contains("(1 frames)"));
    }

    #[test]
    fn a_label_names_the_callsite_in_the_line() {
        const PERF: Wording = Wording {
            tag: "perf",
            rate_unit: "calls/s",
            counted: "calls",
            timed: "build",
        };
        let start = Instant::now();
        let mut m = Meter::new(start);
        assert!(
            m.record(
                start + Duration::from_millis(500),
                Duration::from_millis(2),
                &PERF,
                Some("render")
            )
            .is_none()
        );
        let line = m
            .record(
                start + Duration::from_millis(1000),
                Duration::from_millis(6),
                &PERF,
                Some("render"),
            )
            .expect("crossing the window emits the aggregate line");
        assert_eq!(
            line,
            "[perf] render: 2.0 calls/s over 1.00s (2 calls) | build avg 4.00ms max 6.00ms"
        );
    }
}

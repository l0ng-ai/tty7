//! Per-callsite call-rate and build-time counter, printed once a second to
//! stderr when `TTY7_PROFILE` is set. The meter itself lives in
//! [`crate::core::rate_meter`], shared with `terminal::fps`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::core::rate_meter::{Meter, Wording, flag_enables};

const WORDING: Wording = Wording {
    tag: "perf",
    rate_unit: "calls/s",
    counted: "calls",
    timed: "build",
};

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| flag_enables(std::env::var("TTY7_PROFILE").ok().as_deref()))
}

/// One meter per label: several callsites report here and each wants its own
/// window, so that a slow one is visible rather than averaged away.
fn meters() -> &'static Mutex<HashMap<&'static str, Meter>> {
    static M: OnceLock<Mutex<HashMap<&'static str, Meter>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn record(label: &'static str, build: Duration) {
    let now = Instant::now();
    let mut guard = meters().lock().unwrap();
    let m = guard.entry(label).or_insert_with(|| Meter::new(now));
    if let Some(line) = m.record(now, build, &WORDING, Some(label)) {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wording and the label are this module's contribution to the line;
    /// the windowing behaviour is covered where the meter lives.
    #[test]
    fn the_line_names_the_callsite_and_reads_as_calls_and_build_time() {
        let start = Instant::now();
        let mut m = Meter::new(start);
        assert!(
            m.record(
                start + Duration::from_millis(500),
                Duration::from_millis(2),
                &WORDING,
                Some("render")
            )
            .is_none()
        );
        let line = m
            .record(
                start + Duration::from_millis(1000),
                Duration::from_millis(6),
                &WORDING,
                Some("render"),
            )
            .expect("crossing the window emits the aggregate line");
        assert_eq!(
            line,
            "[perf] render: 2.0 calls/s over 1.00s (2 calls) | build avg 4.00ms max 6.00ms"
        );
    }
}

//! Frame-rate and paint-time counter, printed once a second to stderr when
//! `TTY7_FPS` is set. The meter itself lives in [`crate::core::rate_meter`],
//! shared with `ui::perf`.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::core::rate_meter::{Meter, Wording, flag_enables};

const WORDING: Wording = Wording {
    tag: "fps",
    rate_unit: "fps",
    counted: "frames",
    timed: "paint",
};

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| flag_enables(std::env::var("TTY7_FPS").ok().as_deref()))
}

/// One meter, not one per callsite: there is only ever one thing being
/// measured here, the window's paint.
fn meter() -> &'static Mutex<Option<Meter>> {
    static M: OnceLock<Mutex<Option<Meter>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(None))
}

pub fn record(paint: Duration) {
    let now = Instant::now();
    let mut guard = meter().lock().unwrap();
    let m = guard.get_or_insert_with(|| Meter::new(now));
    if let Some(line) = m.record(now, paint, &WORDING, None) {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wording is the only thing this module contributes to the line, so
    /// it is the only thing worth pinning here — the windowing behaviour is
    /// covered where the meter lives.
    #[test]
    fn the_line_reads_as_frames_and_paint_time() {
        let start = Instant::now();
        let mut m = Meter::new(start);
        assert!(
            m.record(
                start + Duration::from_millis(100),
                Duration::from_millis(2),
                &WORDING,
                None
            )
            .is_none()
        );
        assert!(
            m.record(
                start + Duration::from_millis(200),
                Duration::from_millis(6),
                &WORDING,
                None
            )
            .is_none()
        );
        let line = m
            .record(
                start + Duration::from_millis(1500),
                Duration::from_millis(4),
                &WORDING,
                None,
            )
            .expect("crossing the window emits the aggregate line");
        assert_eq!(
            line,
            "[fps] 2.0 fps over 1.50s (3 frames) | paint avg 4.00ms max 6.00ms"
        );
    }
}

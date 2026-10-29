//! The input-latency probe (perf survey, 2026-10-29): keystroke-to-pixel
//! timestamps behind `TTY59_INPUT_TRACE=1`, one stderr line per stage mark.
//!
//! The stages a keystroke travels: the window's key handler (`key`), the
//! pty write (`write`), the reader's feed of the shell's echo (`feed`),
//! the frame build over the engine (`frame`), and the element's paint
//! (`paint`). Stages cross threads (UI → reader → UI), so marks carry no
//! correlation id — post-processing pairs them by monotonic order: each
//! key's end-to-end latency runs to the first paint after the first feed
//! that follows its write.
//!
//! Kept for perf work; off by default, and a disabled mark costs one load.

use std::sync::OnceLock;
use std::time::Instant;

/// Read once: the marks sit on the reader's feed and the paint path, so a
/// disabled probe must cost one load, not an environment lookup per call.
static START: OnceLock<Option<Instant>> = OnceLock::new();

pub fn enabled() -> bool {
    START
        .get_or_init(|| {
            std::env::var_os("TTY59_INPUT_TRACE")
                .is_some_and(|v| !v.is_empty() && v != "0")
                .then(Instant::now)
        })
        .is_some()
}

/// One stage mark: microseconds since the first mark, on one line.
pub fn mark(stage: &'static str) {
    if !enabled() {
        return;
    }
    let t = START
        .get()
        .copied()
        .flatten()
        .map(|s| s.elapsed().as_micros())
        .unwrap_or_default();
    eprintln!("[inputtrace {stage} {t}]");
}

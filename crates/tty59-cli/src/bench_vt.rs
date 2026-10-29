//! Throughput measurements for the VT engine
//! (docs/libghostty-migration.md): the same byte streams the migration
//! measured through both engines, now through libghostty-vt alone — the
//! comparison arms died with the engine they pinned (Phase 6b); reference
//! numbers from the both-engine era live in the migration doc's phase
//! tables.
//!
//! These are `#[ignore]`d — they take seconds and report, not assert — run
//! them explicitly, in release for numbers that mean anything:
//!
//! ```text
//! TTY59_GHOSTTY_DIR=… cargo test --release -p tty59-cli \
//!     --bin tty59 bench_vt -- --ignored --nocapture
//! ```
//!
//! The workloads mirror `scripts/bench`, which drives a whole terminal
//! through a real window: the plaintext-IO test there `cat`s an ~11 MB text
//! file, and the frame-rate test runs DOOM-fire, a cursor-addressed color
//! animation. Here the same shapes go straight into the parser, which is
//! the slice the migration changed. MB/s is bytes ÷ wall time, chunks fed
//! at 64 KiB like a PTY reader delivers them.

#![cfg(test)]

use std::time::Instant;

/// Lines of 78 columns or less, newline-terminated: the shape of the
/// `cat shakespeare.txt` corpus (a wrapped line is still one write of
/// plain bytes; the grid does the wrapping).
fn workload_plaintext() -> Vec<u8> {
    // A deterministic shuffle of a small word list — no network, no corpus
    // dependency, and byte-for-byte reproducible across runs.
    const WORDS: &[&str] = &[
        "to", "be", "or", "not", "to", "be", "that", "is", "the", "question",
        "whether", "tis", "nobler", "in", "the", "mind", "to", "suffer",
        "the", "slings", "and", "arrows", "of", "outrageous", "fortune",
        "or", "to", "take", "arms", "against", "a", "sea", "of", "troubles",
        "and", "by", "opposing", "end", "them", "to", "die", "to", "sleep",
    ];
    let mut out = Vec::with_capacity(11 << 20);
    let mut line = String::new();
    let mut state = 0x5EED_u64;
    let mut word = 0usize;
    while out.len() < 11 << 20 {
        // xorshift keeps every word reachable without a real RNG.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        word = (word.wrapping_add(state as usize)) % WORDS.len();
        let w = WORDS[word];
        if line.len() + w.len() + 1 > 78 {
            out.extend_from_slice(line.as_bytes());
            out.extend_from_slice(b"\r\n");
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(w);
    }
    out
}

/// Full-screen color frames like DOOM-fire's: clear, address every row,
/// paint it a color, repeat. Heavier per byte than plaintext — cursor
/// addressing, SGR churn, and a clear that scrolls a screenful into
/// history each frame.
fn workload_tui() -> Vec<u8> {
    const COLS: usize = 200;
    const ROWS: usize = 50;
    let mut out = Vec::with_capacity(11 << 20);
    let mut frame = 0u32;
    while out.len() < 11 << 20 {
        out.extend_from_slice(b"\x1b[H\x1b[2J");
        for row in 0..ROWS {
            out.extend_from_slice(format!("\x1b[{};1H\x1b[48;5;{}m", row + 1, (frame + row as u32) % 256).as_bytes());
            out.extend(std::iter::repeat(b' ').take(COLS - 12));
            out.extend_from_slice(format!("f{:06}", frame).as_bytes());
            out.extend_from_slice(b"\x1b[0m");
        }
        frame = frame.wrapping_add(1);
    }
    out
}

/// ~11 MB of bytes, in reader-sized chunks.
fn chunks(bytes: &[u8]) -> Vec<&[u8]> {
    bytes.chunks(64 * 1024).collect()
}

fn run_engine(bytes: &[u8]) -> f64 {
    let mut term = tty59_vt::Terminal::new(200, 50, 10_000).expect("terminal");
    let t0 = Instant::now();
    for chunk in chunks(bytes) {
        term.write(chunk);
    }
    t0.elapsed().as_secs_f64()
}

fn report(name: &str, bytes: &[u8], secs: f64) {
    let mb = bytes.len() as f64 / 1e6;
    eprintln!("{name}: {mb:.1} MB — ghostty {:.0} MB/s ({secs:.2}s)", mb / secs);
}

#[test]
#[ignore = "throughput measurement, not an assertion (see module docs)"]
fn bench_plaintext_io() {
    let bytes = workload_plaintext();
    report("plaintext", &bytes, run_engine(&bytes));
}

#[test]
#[ignore = "throughput measurement, not an assertion (see module docs)"]
fn bench_tui_frames() {
    let bytes = workload_tui();
    report("tui", &bytes, run_engine(&bytes));
}

/// Snapshot cost for the GUI's worst common case: a full 200×50 viewport of
/// styled cells, taken after streaming (scrollback deep, viewport full).
#[test]
#[ignore = "latency measurement, not an assertion (see module docs)"]
fn bench_snapshot_200x50() {
    let mut term = tty59_vt::Terminal::new(200, 50, 10_000).expect("terminal");
    term.write(&workload_tui()[..2 << 20]);

    // Warm once (allocator, any first-touch pages), then time ten.
    let _ = term.snapshot();
    let mut worst = std::time::Duration::ZERO;
    let mut total = std::time::Duration::ZERO;
    for _ in 0..10 {
        let t0 = Instant::now();
        let snap = term.snapshot();
        let dt = t0.elapsed();
        assert_eq!(snap.screen_rows, 50);
        worst = worst.max(dt);
        total += dt;
    }
    eprintln!(
        "snapshot 200×50: mean {:.2} ms, worst {:.2} ms (10 runs)",
        total.as_secs_f64() * 100.0,
        worst.as_secs_f64() * 1000.0,
    );

    // The capture path's extraction cost over the same grid depth.
    let t0 = Instant::now();
    let text = term.text_full();
    eprintln!(
        "text_full ({} rows incl. scrollback, {} chars): {:.2} ms",
        term.scrollback().0,
        text.chars().count(),
        t0.elapsed().as_secs_f64() * 1000.0,
    );
}

/// Text extraction at real scrollback depth: plaintext keeps everything in
/// history, unlike the TUI workload above, whose `ESC[2J` scrolls a
/// screenful per frame but the rewriter routes to the scroll-clear. This
/// is the `capture --plain` worst case.
#[test]
#[ignore = "latency measurement, not an assertion (see module docs)"]
fn bench_text_extraction_at_scrollback_depth() {
    let bytes = &workload_plaintext()[..4 << 20];

    let mut term = tty59_vt::Terminal::new(200, 50, 10_000).expect("terminal");
    for chunk in chunks(bytes) {
        term.write(chunk);
    }
    let (rows, scrollback) = term.scrollback();
    let t0 = Instant::now();
    let text = term.text_full();
    eprintln!(
        "text_full ({} rows, {} scrollback, {} chars): {:.1} ms",
        rows,
        scrollback,
        text.chars().count(),
        t0.elapsed().as_secs_f64() * 1000.0,
    );
}

//! The reader loop's parse-path throughput bench
//! (docs/libghostty-migration.md): the facade's `advance` — the one call
//! between the pane's bytes and the engine — over ~11 MB workloads.
//!
//! `#[ignore]`d like the other benches — it takes seconds and reports, not
//! asserts — meant for release:
//!
//! ```text
//! cargo test --release -p tty59-vt reader_bench -- --ignored --nocapture
//! ```
//!
//! The workloads match `crates/tty59-cli/src/bench_vt.rs` (the shapes of
//! `scripts/bench`: a `cat` of wrapped plaintext, and DOOM-fire-style
//! full-screen color frames), copied so this crate need not depend on
//! tty59-cli. Chunks are fed at 64 KiB, the size a PTY reader delivers.
//!
//! **Recorded history:** the migration's gate was the facade's overhead
//! against a raw engine drive — 0.99–1.01× on the replaced engine, and
//! engine-vs-engine after the flip (that arm died with the engine it
//! pinned). This bench remains as the absolute-throughput watch: reference
//! numbers live in the migration doc's phase tables.

use std::hint::black_box;
use std::time::Instant;

use crate::VtTerm;
use crate::model::VtOptions;

const COLS: usize = 200;
const ROWS: usize = 50;
const SCROLLBACK: usize = 10_000;

/// Lines of 78 columns or less, newline-terminated: the shape of the
/// `cat shakespeare.txt` corpus (a wrapped line is still one write of
/// plain bytes; the grid does the wrapping).
fn workload_plaintext() -> Vec<u8> {
    // A deterministic shuffle of a small word list — no network, no corpus
    // dependency, and byte-for-byte reproducible across runs. The same
    // generator as `bench_vt.rs`, so the numbers stay comparable.
    const WORDS: &[&str] = &[
        "to",
        "be",
        "or",
        "not",
        "to",
        "be",
        "that",
        "is",
        "the",
        "question",
        "whether",
        "tis",
        "nobler",
        "in",
        "the",
        "mind",
        "to",
        "suffer",
        "the",
        "slings",
        "and",
        "arrows",
        "of",
        "outrageous",
        "fortune",
        "or",
        "to",
        "take",
        "arms",
        "against",
        "a",
        "sea",
        "of",
        "troubles",
        "and",
        "by",
        "opposing",
        "end",
        "them",
        "to",
        "die",
        "to",
        "sleep",
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
    let mut out = Vec::with_capacity(11 << 20);
    let mut frame = 0u32;
    while out.len() < 11 << 20 {
        out.extend_from_slice(b"\x1b[H\x1b[2J");
        for row in 0..ROWS {
            out.extend_from_slice(
                format!(
                    "\x1b[{};1H\x1b[48;5;{}m",
                    row + 1,
                    (frame + row as u32) % 256
                )
                .as_bytes(),
            );
            out.extend(std::iter::repeat_n(b' ', COLS - 12));
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

/// The reader loop's cost, engine owned and parsed through the facade.
fn run_facade(bytes: &[u8]) -> f64 {
    let mut term = VtTerm::with_options(
        VtOptions {
            scrollback_lines: SCROLLBACK,
            ..VtOptions::default()
        },
        COLS as u16,
        ROWS as u16,
    );
    let t0 = Instant::now();
    for chunk in chunks(bytes) {
        term.advance(chunk);
    }
    black_box(&term);
    t0.elapsed().as_secs_f64()
}

/// One workload, rounds and medians — a mean would let one stalled round
/// bury the signal.
fn bench(name: &str, bytes: &[u8]) {
    // Warm the arm (allocator, branch predictors, first-touch pages).
    let warm = &bytes[..(1 << 20).min(bytes.len())];
    run_facade(warm);

    let rounds = 7;
    let mut times = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t0 = Instant::now();
        run_facade(bytes);
        let t1 = Instant::now();
        times.push(t1 - t0);
    }
    let median = |times: &mut Vec<std::time::Duration>| {
        times.sort();
        times[times.len() / 2].as_secs_f64()
    };
    let s = median(&mut times);
    let mb = bytes.len() as f64 / 1e6;
    eprintln!(
        "{name}: {mb:.1} MB — facade advance {:.0} MB/s ({s:.2}s)",
        mb / s
    );
}

#[test]
#[ignore = "throughput measurement, not an assertion (see module docs)"]
fn bench_reader_plaintext() {
    bench("reader plaintext", &workload_plaintext());
}

#[test]
#[ignore = "throughput measurement, not an assertion (see module docs)"]
fn bench_reader_tui() {
    bench("reader tui", &workload_tui());
}

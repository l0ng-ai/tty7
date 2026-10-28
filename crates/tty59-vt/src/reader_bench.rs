//! Phase 2e throughput measurement for the libghostty-vt migration
//! (docs/libghostty-migration.md): the reader loop's parse path, raw
//! `Processor::advance` on a bare engine terminal against the facade's
//! `VtTerm::advance` — the one call the seam puts between the pane's bytes
//! and the engine.
//!
//! `#[ignore]`d like the other benches — it takes seconds and reports, not
//! asserts — meant for release:
//!
//! ```text
//! cargo test --release -p tty59-vt reader_bench -- --ignored --nocapture
//! ```
//!
//! The workloads are the ones `crates/tty59-cli/src/bench_vt.rs` drives the
//! Phase 0 comparison with (the shapes of `scripts/bench`: a `cat` of
//! ~11 MB of wrapped plaintext, and DOOM-fire-style full-screen color
//! frames), copied so this crate need not depend on tty59-cli. Chunks are
//! fed at 64 KiB, the size a PTY reader delivers. The gate is ~2%: the
//! facade's `advance` is an `#[inline]` forward, so anything past noise is
//! a real regression and a blocker.

use std::hint::black_box;
use std::time::Instant;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

use crate::model::VtOptions;
use crate::VtTerm;

const COLS: usize = 200;
const ROWS: usize = 50;
const SCROLLBACK: usize = 10_000;

/// What `Term::new` needs to know about the grid it is filling. The raw arm
/// drives the engine directly (that is what it measures), so it keeps its
/// own shim; the facade arm sizes its grid through `VtTerm`, which applies
/// the same floor.
struct BenchSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for BenchSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Lines of 78 columns or less, newline-terminated: the shape of the
/// `cat shakespeare.txt` corpus (a wrapped line is still one write of
/// plain bytes; the grid does the wrapping).
fn workload_plaintext() -> Vec<u8> {
    // A deterministic shuffle of a small word list — no network, no corpus
    // dependency, and byte-for-byte reproducible across runs. The same
    // generator as `bench_vt.rs`, so the numbers stay comparable.
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
    let mut out = Vec::with_capacity(11 << 20);
    let mut frame = 0u32;
    while out.len() < 11 << 20 {
        out.extend_from_slice(b"\x1b[H\x1b[2J");
        for row in 0..ROWS {
            out.extend_from_slice(
                format!("\x1b[{};1H\x1b[48;5;{}m", row + 1, (frame + row as u32) % 256).as_bytes(),
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

/// The reader loop before the seam: a bare engine terminal, the parser
/// advanced by hand, one chunk per call.
fn run_raw(bytes: &[u8]) -> f64 {
    let mut term = Term::new(
        Config { scrolling_history: SCROLLBACK, ..Config::default() },
        &(BenchSize { cols: COLS, rows: ROWS }),
        VoidListener,
    );
    let mut parser: Processor = Processor::new();
    let t0 = Instant::now();
    for chunk in chunks(bytes) {
        parser.advance(&mut term, chunk);
    }
    black_box(&term);
    t0.elapsed().as_secs_f64()
}

/// The reader loop after: the same engine, owned and parsed through the
/// facade.
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

/// One workload through both arms, interleaved a round at a time so machine
/// noise hits both equally; medians, because a mean would let one stalled
/// round bury the signal.
fn bench(name: &str, bytes: &[u8]) {
    // Warm both arms (allocator, branch predictors, first-touch pages).
    let warm = &bytes[..(1 << 20).min(bytes.len())];
    run_raw(warm);
    run_facade(warm);

    let rounds = 7;
    let mut raw_times = Vec::with_capacity(rounds);
    let mut facade_times = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t0 = Instant::now();
        run_raw(bytes);
        let t1 = Instant::now();
        run_facade(bytes);
        let t2 = Instant::now();
        raw_times.push(t1 - t0);
        facade_times.push(t2 - t1);
    }
    let median = |times: &mut Vec<std::time::Duration>| {
        times.sort();
        times[times.len() / 2].as_secs_f64()
    };
    let (raw_s, facade_s) = (median(&mut raw_times), median(&mut facade_times));
    let mb = bytes.len() as f64 / 1e6;
    eprintln!(
        "{name}: {mb:.1} MB — raw Processor::advance {:.0} MB/s ({raw_s:.2}s), \
         facade advance {:.0} MB/s ({facade_s:.2}s), facade/raw {:.3}×",
        mb / raw_s,
        mb / facade_s,
        facade_s / raw_s,
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

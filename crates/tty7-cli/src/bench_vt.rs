//! Phase 0 throughput measurements for the libghostty-vt migration
//! (docs/libghostty-migration.md): the same byte streams pushed through
//! `alacritty_terminal::Processor` and `tty7_vt::Terminal::write`, timed
//! side by side.
//!
//! These are `#[ignore]`d — they take seconds and report, not assert — run
//! them explicitly, in release for numbers that mean anything:
//!
//! ```text
//! TTY7_GHOSTTY_DIR=… cargo test --release -p tty7-cli \
//!     --features vt-ghostty --bin tty7 bench_vt -- --ignored --nocapture
//! ```
//!
//! The workloads mirror `scripts/bench`, which drives a whole terminal
//! through a real window: the plaintext-IO test there `cat`s an ~11 MB text
//! file, and the frame-rate test runs DOOM-fire, a cursor-addressed color
//! animation. Here the same shapes go straight into the parser, which is
//! the slice the migration changes. MB/s is bytes ÷ wall time, chunks fed
//! at 64 KiB like a PTY reader delivers them.

#![cfg(all(test, feature = "vt-ghostty"))]

use std::time::Instant;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

use crate::screen::GridSize;
use tty7_core::daemon::protocol::WinSize;

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

fn win_size() -> WinSize {
    WinSize { cols: 200, rows: 50, cell_w: 8, cell_h: 16 }
}

fn run_alacritty(bytes: &[u8]) -> f64 {
    let grid = GridSize::of(win_size());
    let mut term = Term::new(Config::default(), &grid, VoidListener);
    let mut parser: Processor = Processor::new();
    let t0 = Instant::now();
    for chunk in chunks(bytes) {
        parser.advance(&mut term, chunk);
    }
    t0.elapsed().as_secs_f64()
}

fn run_ghostty(bytes: &[u8]) -> f64 {
    let mut term = tty7_vt::Terminal::new(200, 50, 10_000).expect("terminal");
    let t0 = Instant::now();
    for chunk in chunks(bytes) {
        term.write(chunk);
    }
    t0.elapsed().as_secs_f64()
}

fn report(name: &str, bytes: &[u8], alacritty_secs: f64, ghostty_secs: f64) {
    let mb = bytes.len() as f64 / 1e6;
    eprintln!(
        "{name}: {mb:.1} MB — alacritty {:.0} MB/s ({alacritty_secs:.2}s), \
         ghostty {:.0} MB/s ({ghostty_secs:.2}s), ratio ghostty/alacritty {:.2}×",
        mb / alacritty_secs,
        mb / ghostty_secs,
        (mb / ghostty_secs) / (mb / alacritty_secs),
    );
}

#[test]
#[ignore = "throughput measurement, not an assertion (see module docs)"]
fn bench_plaintext_io() {
    let bytes = workload_plaintext();
    report("plaintext", &bytes, run_alacritty(&bytes), run_ghostty(&bytes));
}

#[test]
#[ignore = "throughput measurement, not an assertion (see module docs)"]
fn bench_tui_frames() {
    let bytes = workload_tui();
    report("tui", &bytes, run_alacritty(&bytes), run_ghostty(&bytes));
}

/// Snapshot cost for the GUI's worst common case: a full 200×50 viewport of
/// styled cells, taken after streaming (scrollback deep, viewport full).
#[test]
#[ignore = "latency measurement, not an assertion (see module docs)"]
fn bench_snapshot_200x50() {
    let mut term = tty7_vt::Terminal::new(200, 50, 10_000).expect("terminal");
    term.write(&workload_tui()[..2 << 20]);

    // Warm once (allocator, any first-touch pages), then time ten.
    let _ = term.snapshot();
    let mut worst = std::time::Duration::ZERO;
    let mut total = std::time::Duration::ZERO;
    for _ in 0..10 {
        let t0 = Instant::now();
        let snap = term.snapshot();
        let dt = t0.elapsed();
        assert_eq!(snap.rows, 50);
        worst = worst.max(dt);
        total += dt;
    }
    eprintln!(
        "snapshot 200×50: mean {:.2} ms, worst {:.2} ms (10 runs)",
        total.as_secs_f64() * 100.0,
        worst.as_secs_f64() * 1000.0,
    );

    // The capture path's extraction cost, against alacritty's
    // bounds_to_string over the same grid depth.
    let t0 = Instant::now();
    let text = term.text_full();
    eprintln!(
        "text_full ({} rows incl. scrollback, {} chars): {:.2} ms",
        term.scrollback().0,
        text.chars().count(),
        t0.elapsed().as_secs_f64() * 1000.0,
    );

    let grid = GridSize::of(win_size());
    let mut aterm = Term::new(Config::default(), &grid, VoidListener);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut aterm, &workload_tui()[..2 << 20]);
    let start = alacritty_terminal::index::Point::new(
        aterm.topmost_line(),
        alacritty_terminal::index::Column(0),
    );
    let end = alacritty_terminal::index::Point::new(
        aterm.bottommost_line(),
        aterm.last_column(),
    );
    let t0 = Instant::now();
    let atext = aterm.bounds_to_string(start, end);
    eprintln!(
        "bounds_to_string ({} chars): {:.2} ms",
        atext.chars().count(),
        t0.elapsed().as_secs_f64() * 1000.0,
    );
}

/// Text extraction at real scrollback depth: plaintext keeps everything in
/// history, unlike the TUI workload above, whose `ESC[2J` clears without
/// scrolling in ghostty while alacritty's ED 2 pushes the screen into
/// scrollback — so the TUI sample cannot show extraction at depth. This is
/// the `capture --plain` worst case.
#[test]
#[ignore = "latency measurement, not an assertion (see module docs)"]
fn bench_text_extraction_at_scrollback_depth() {
    let bytes = &workload_plaintext()[..4 << 20];

    let mut term = tty7_vt::Terminal::new(200, 50, 10_000).expect("terminal");
    for chunk in chunks(bytes) {
        term.write(chunk);
    }
    let (rows, scrollback) = term.scrollback();
    let t0 = Instant::now();
    let text = term.text_full();
    eprintln!(
        "ghostty text_full ({} rows, {} scrollback, {} chars): {:.1} ms",
        rows,
        scrollback,
        text.chars().count(),
        t0.elapsed().as_secs_f64() * 1000.0,
    );

    let grid = GridSize::of(win_size());
    let mut aterm = Term::new(Config::default(), &grid, VoidListener);
    let mut parser: Processor = Processor::new();
    for chunk in chunks(bytes) {
        parser.advance(&mut aterm, chunk);
    }
    let start = alacritty_terminal::index::Point::new(
        aterm.topmost_line(),
        alacritty_terminal::index::Column(0),
    );
    let end = alacritty_terminal::index::Point::new(
        aterm.bottommost_line(),
        aterm.last_column(),
    );
    let t0 = Instant::now();
    let atext = aterm.bounds_to_string(start, end);
    eprintln!(
        "alacritty bounds_to_string ({} chars): {:.1} ms",
        atext.chars().count(),
        t0.elapsed().as_secs_f64() * 1000.0,
    );
}

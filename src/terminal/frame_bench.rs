//! The frame-fill latency bench (docs/libghostty-migration.md): what one
//! paint's grid build costs for a 200×50 viewport of mixed-SGR cells,
//! through the seam onto libghostty-vt.
//!
//! `#[ignore]`d like `crates/tty59-cli/src/bench_vt.rs` — it reports, not
//! asserts — and meant for release:
//!
//! ```text
//! cargo test --release --bin tty59 frame_bench -- --ignored --nocapture
//! ```
//!
//! **The recorded gate.** The migration's frame gate was ≤1.10× the
//! pre-migration direct engine walk, measured at **0.209 ms** for this
//! workload on the migration machine (commit d921d75's render path, the
//! alacritty engine — the arm was deleted with that engine in Phase 6b).
//! The libghostty-vt fill measured 0.217–0.220 ms through Phases 3–6
//! (1.04–1.05×), steady 0.005 ms. That is the bar this bench watches: a
//! median far above ~0.23 ms means the fill regressed, not the machine.
//!
//! What the workload leaves out, deliberately: OSC 8 links and SGR 2 dim
//! runs. Their paint cost lives in `osc8_underline` and the legibility
//! solve, which the migration did not touch.

use std::hint::black_box;
use std::time::Instant;

use tty59_vt::VtTerm;
use tty59_vt::model::Frame;

const COLS: usize = 200;
const ROWS: usize = 50;

/// The mixed-SGR screen the fill walks: bold/colored labels, truecolor,
/// inverse video, every underline style, wide CJK runs, and plain text —
/// rotating per row so no row's cells resolve the same way.
fn workload_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    for row in 0..ROWS {
        match row % 5 {
            0 => {
                bytes.extend_from_slice(b"\x1b[1;38;5;45m");
                for _ in 0..COLS / 8 {
                    bytes.extend_from_slice("label·· ".as_bytes());
                }
            }
            1 => {
                bytes.extend_from_slice(b"\x1b[38;2;170;120;90m");
                for _ in 0..COLS / 10 {
                    bytes.extend_from_slice(b"truecolor ");
                }
            }
            2 => {
                bytes.extend_from_slice(b"\x1b[7;48;5;17m");
                bytes.extend(std::iter::repeat_n(b'i', COLS));
            }
            3 => {
                bytes.extend_from_slice(b"\x1b[4;58;5;99m");
                bytes.extend(b"underlined ".iter().cycle().take(COLS));
            }
            _ => {
                bytes.extend_from_slice(b"\x1b[0m");
                // Four wide characters and a space: nine cells an iterate.
                for _ in 0..COLS / 9 {
                    bytes.extend_from_slice("語彙力測 ".as_bytes());
                }
            }
        }
        bytes.extend_from_slice(b"\x1b[0m\r\n");
    }
    bytes
}

/// The frame fill, two regimes —
///
/// - **full**: the frame reused, the whole screen rewritten between fills
///   (every row lands dirty; the fill copies all of them). This is the
///   cost the frame gate budgets: the first fill after a write burst, a
///   resize, or any consumer handing the engine a new buffer.
/// - **steady**: one reused `Frame`, and between fills exactly one row is
///   rewritten — the dirty-tracking regime, where clean rows are skipped
///   and the fill pays for one row's cells plus the walk.
#[test]
#[ignore = "latency measurement, not an assertion (see module docs)"]
fn bench_frame_build_200x50() {
    let mut term = VtTerm::with_options(
        tty59_vt::model::VtOptions {
            scrollback_lines: 10_000,
            ..tty59_vt::model::VtOptions::default()
        },
        COLS as u16,
        ROWS as u16,
    );
    // Without the workload's trailing CRLF: it opens one row too many and
    // scrolls the first off the viewport — this bench just wants rows
    // 0..49 on screen.
    let bytes = workload_bytes();
    term.feed(&bytes[..bytes.len() - 2]);

    // Sanity: the fill crosses the cells the workload wrote.
    {
        let mut frame = Frame::default();
        term.view().frame(&mut frame, false);
        assert_eq!(frame.rows.len(), ROWS);
        assert_eq!(frame.rows[0].cells[0].c, 'l', "the styled label cell");
        assert!(frame.rows[0].cells[0].bold);
        assert_eq!(frame.rows[2].cells[100].c, 'i', "the inverse row cell");
    }

    // Warm the render state, the iterators, and the allocator.
    let mut warm = Frame::default();
    for _ in 0..10 {
        term.view().frame(&mut warm, false);
    }

    let redraw = |frame_no: u32| {
        let mut bytes = Vec::with_capacity(8 * COLS * ROWS);
        for row in 0..ROWS {
            bytes.extend_from_slice(
                format!(
                    "\x1b[{row};1H\x1b[1;38;5;{}m",
                    (frame_no as usize + row) % 256
                )
                .as_bytes(),
            );
            bytes.extend(std::iter::repeat(b'x').take(COLS));
            bytes.extend_from_slice(b"\x1b[0m");
        }
        bytes
    };

    // The regimes run as separate phases, not interleaved rounds: the
    // dirty-skip is keyed to *this frame* having seen the previous fill,
    // and two frames alternating over one engine rightly pay full copies
    // for the one that went stale between its fills. One pane, one frame,
    // is the shape the steady number describes.
    let rounds = 300;
    let mut full_times = Vec::with_capacity(rounds);
    let mut full_frame = Frame::default();
    let mut frame_no = 0u32;
    for _ in 0..rounds {
        frame_no = frame_no.wrapping_add(1);
        term.feed(&redraw(frame_no));
        let t0 = Instant::now();
        term.view().frame(&mut full_frame, false);
        let t1 = Instant::now();
        black_box(&full_frame);
        full_times.push(t1 - t0);
    }

    let mut steady_times = Vec::with_capacity(rounds);
    let mut steady_frame = Frame::default();
    // One fill to stamp the steady frame as current, then the regime:
    // rewriting one row the way a status line repaints (its row and the
    // cursor's land dirty next fill, everything else skips).
    term.view().frame(&mut steady_frame, false);
    let touch_row = format!("\x1b[{row};1H\x1b[1;38;5;45m", row = ROWS / 2 + 1);
    let mut touch = 0u32;
    for _ in 0..rounds {
        touch = touch.wrapping_add(1);
        term.feed(format!("{touch_row}{touch:06}").as_bytes());
        let t2 = Instant::now();
        term.view().frame(&mut steady_frame, false);
        let t3 = Instant::now();
        black_box(&steady_frame);
        steady_times.push(t3 - t2);
    }

    // Informational: a consumer handing the engine a brand-new frame pays
    // its allocation once — the number to know before "optimizing" the fill.
    let mut alloc_times = Vec::with_capacity(64);
    for _ in 0..64 {
        let t0 = Instant::now();
        let mut fresh = Frame::default();
        term.view().frame(&mut fresh, false);
        let t1 = Instant::now();
        black_box(&fresh);
        alloc_times.push(t1 - t0);
    }

    let median = |times: &mut Vec<std::time::Duration>| {
        times.sort();
        times[times.len() / 2].as_secs_f64() * 1000.
    };
    eprintln!(
        "frame fill 200×50, median of {rounds}: full-dirty (screen \
         rewritten, frame reused) {:.3} ms; steady (one row changed) {:.3} ms; \
         fresh frame (alloc, informational) {:.3} ms \
         (gate: 0.209 ms pre-migration baseline, 0.217–0.220 through the migration)",
        median(&mut full_times),
        median(&mut steady_times),
        median(&mut alloc_times),
    );
}

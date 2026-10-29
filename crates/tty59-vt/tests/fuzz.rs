//! The Phase 3 fuzz sweep: hostile input through both backends, with the
//! seam's invariants checked after every step — no panics, no hangs, and
//! whatever the parser makes of the garbage, the grid stays a grid.
//!
//! The engines are not compared with each other here (arbitrary bytes can
//! legitimately land in different-but-valid states; agreement on meaningful
//! streams is the differential harness's job). What is asserted:
//!
//! - the cursor stays inside the grid (`x < columns`, `y < screen_lines`);
//! - a frame fill yields exactly `screen_lines` rows of exactly `columns`
//!   cells, whatever the resize history;
//! - the cursor's cell and the whole-grid text extraction never panic;
//! - deep stacks (kitty pushes, SGR push/pop, title floods) leave the
//!   reader alive — fork patch #1's guard, run against both engines.
//!
//! Deterministic: the seed is fixed, so a failure reproduces by rerunning.
//! A longer `#[ignore]`d variant exists for leisurely runs.
//!
//! ```text
//! cargo test -p tty59-vt --features ghostty --test fuzz
//! ```

#![cfg(feature = "ghostty")]

use tty59_vt::model::{Frame, GridPoint, VtOptions};
use tty59_vt::{AlacTerm, VtEvent, VtEventListener, VtTerm};

/// A no-op listener: fuzzing watches the grid, not the effects.
#[derive(Clone, Default)]
struct Nop;
impl VtEventListener for Nop {
    fn send_event(&self, _event: VtEvent) {}
}

/// The deterministic generator: xorshift, because reproduction beats
/// quality here.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Fragments the sweep stitches in: real sequences cut at hostile points,
/// plus the mode churn a TUI produces. Each is valid or near-valid; the
/// *splits* are where the fuzz lives.
const FRAGMENTS: &[&[u8]] = &[
    b"\x1b[2J\x1b[H",
    b"\x1b[?1049h",
    b"\x1b[?1049l",
    b"\x1b[?2026h",
    b"\x1b[?2026l",
    b"\x1b[?2027h",
    b"\x1b[?2027l",
    b"\x1b[>1u",
    b"\x1b[>25u",
    b"\x1b[<u",
    b"\x1b[=0;1u",
    b"\x1b[1;31;48;5;99m",
    b"\x1b[0m",
    b"\x1b[38;2;1;2;3m",
    b"\x1b[4:3m",
    b"\x1b[58;2;9;8;7m",
    b"\x1b[2;10r",
    b"\x1b[r",
    b"\x1b[5;13H",
    b"\x1b[3;1;79;9;4;3*|",
    b"\x1b]0;a title\x07",
    b"\x1b]8;;https://e.com\x07link\x1b]8;;\x07",
    b"\x1b]52;c;AAAA\x07",
    b"\x1bP1;2;3|payload\x1b\\",
    b"\x1b_&appl..\x1b\\",
    b"\x1b[6n",
    b"\x1b[c",
    b"\x1b[>q",
    b"\x1b#8",
    b"\x1b(B",
    b"\x1bM",
    b"\x1bD",
    b"\x1bE",
    b"\x1b7\x1b8",
    b"tab\there\tand\r\n",
    "wide \u{4f60}\u{597d} emoji \u{2764}\u{fe0f}\r\n".as_bytes(),
    "marks e\u{301}\u{301}\r\n".as_bytes(),
    b"\x1b[10;20H\x1b[K\x1b[1J",
    b"\x1b[2;3Su000",
    b"\x1b[?1000;1002;1006h",
    b"\x1b[?1000l",
];

/// One sweep of `steps` actions over both engines, invariant-checked after
/// every action. Returns the actions as a rendered trace on failure.
fn sweep(seed: u64, steps: usize) {
    let mut rng = Rng(seed);
    let cols = 20 + rng.below(41) as u16;
    let rows = 6 + rng.below(15) as u16;
    // The sweep's own budget: a bounded scrollback (garbage feeds grow it,
    // and a 10k-line default under two engines is memory for nothing here)
    // and the default everything else. The systemd-run memory cap around
    // the test is the backstop, not the fix.
    let options = VtOptions { scrollback_lines: 2_000, ..VtOptions::default() };
    let mut alac = AlacTerm::with_listener(options.clone(), cols, rows, Nop);
    let mut ghost = VtTerm::with_listener(options, cols, rows, Nop);

    for step in 0..steps {
        let choice = rng.below(10);
        match choice {
            // Garbage: uniform bytes, escape-dense half the time. The
            // per-step cap keeps a sweep's total input a known quantity.
            0..=2 => {
                let len = 1 + rng.below(128) as usize;
                let mut bytes = vec![0u8; len];
                for b in &mut bytes {
                    *b = if rng.below(2) == 0 {
                        rng.below(256) as u8
                    } else {
                        // Escape-dense: control bytes and sequence pieces.
                        [0x1b, b'[', b']', b';', b'?', b'P', 0x07, b'0', b'm', b'~']
                            [rng.below(10) as usize]
                    };
                }
                alac.feed(&bytes);
                ghost.feed(&bytes);
            }
            // A real fragment, cut at a random point, continued next time
            // it is picked: splits mid-sequence land here naturally.
            3..=5 => {
                let fragment = FRAGMENTS[rng.below(FRAGMENTS.len() as u64) as usize];
                let cut = rng.below(fragment.len() as u64 + 1) as usize;
                alac.feed(&fragment[..cut]);
                ghost.feed(&fragment[..cut]);
                if cut < fragment.len() {
                    // Feed the rest in the same step, split again: the
                    // parser meets the sequence in two pieces either way.
                    let tail = &fragment[cut..];
                    let cut2 = rng.below(tail.len() as u64 + 1) as usize;
                    alac.feed(&tail[..cut2]);
                    ghost.feed(&tail[..cut2]);
                    alac.feed(&tail[cut2..]);
                    ghost.feed(&tail[cut2..]);
                }
            }
            // A resize mid-stream: any size up to the sweep's ceiling,
            // including the floors.
            6..=7 => {
                let cols = 1 + rng.below(120) as u16;
                let rows = 1 + rng.below(40) as u16;
                alac.resize(cols, rows);
                ghost.resize(cols, rows);
            }
            // Deep stacks: kitty pushes and pops far past any real
            // program, built once and fed by reference so the flood is one
            // allocation, not one per repeat element.
            8 => {
                let depth = 64 + rng.below(5_000);
                let flood = b"\x1b[>1u".repeat(depth as usize);
                alac.feed(&flood);
                ghost.feed(&flood);
                let pops = b"\x1b[<u".repeat(depth as usize);
                alac.feed(&pops);
                ghost.feed(&pops);
            }
            // Title floods (fork patch #1's shape) and SGR stack churn.
            _ => {
                let flood = b"\x1b]0;t\x07".repeat(4_096);
                alac.feed(&flood);
                ghost.feed(&flood);
                let sgr = b"\x1b[#p".repeat(1_024);
                alac.feed(&sgr);
                ghost.feed(&sgr);
                alac.feed(&b"\x1b[#q".repeat(1_024));
                ghost.feed(&b"\x1b[#q".repeat(1_024));
            }
        }

        // The invariants, on both engines — through the seam's read trait,
        // which is the surface the walkers and the renderer share. A panic
        // names the engine, step, and seed; rerun with the seed to
        // reproduce.
        let want_sliver = rng.below(2) == 0;
        check("alacritty", &alac.view(), want_sliver, step, seed);
        check("ghostty", &ghost.view(), want_sliver, step, seed);
        // Whole-grid text extraction walks every cell (the Ghostty side is
        // per-cell FFI), so the sweep samples it rather than paying it
        // every step — the cursor/frame invariants above are the per-step
        // contract.
        if step % 16 == 0 {
            let _ = alac.text_full();
            let _ = ghost.text_full();
        }
    }
}

/// The per-step invariants over one engine's read surface.
fn check(
    name: &str,
    view: &tty59_vt::VtTermView<'_>,
    want_sliver: bool,
    step: usize,
    seed: u64,
) {
    let (cols, rows) = (view.columns(), view.screen_lines());
    assert!(cols >= 1 && rows >= 1, "{name}: grid degenerate at step {step}");

    let cursor = view.cursor();
    assert!(
        (usize::from(cursor.x), usize::from(cursor.y)) < (cols, rows),
        "{name}: cursor ({}, {}) outside {}×{} at step {step} (seed {seed})",
        cursor.x, cursor.y, cols, rows,
    );

    // The frame is the renderer's contract: the viewport shape, exactly,
    // whatever the resize history did.
    let mut frame = Frame::default();
    view.frame(&mut frame, want_sliver);
    assert_eq!(
        frame.rows.len(),
        rows,
        "{name}: frame holds {} rows for a {}-row grid at step {step} (seed {seed})",
        frame.rows.len(),
        rows,
    );
    for (row_no, row) in frame.rows.iter().enumerate() {
        assert_eq!(
            row.cells.len(),
            cols,
            "{name}: frame row {row_no} holds {} cells for a {}-column grid at step {step} (seed {seed})",
            row.cells.len(),
            cols,
        );
    }

    // The reads the walkers do.
    let _ = view.cursor_cell();
    let _ = view.glyph(GridPoint { line: -(view.history_size() as i32), col: 0 });
}

#[test]
fn fuzz_both_backends_survive_hostile_input() {
    // A spread of seeds rather than one long run: different state machines
    // per seed, same total budget. Sized for a debug build to stay inside
    // CI's patience (~10 s measured); release multiplies it by an order.
    for seed in 0x9E37_79B9..0x9E37_79B9 + 4 {
        sweep(seed, 40);
    }
}

/// The long variant: the same sweep, minutes of it, for when something
/// only dies deep in. Release build, ignored:
///
/// ```text
/// systemd-run --user --scope -q -p MemoryMax=4G -p MemorySwapMax=0 \
///   timeout 300 cargo test --release -p tty59-vt --features ghostty \
///   --test fuzz -- --ignored --nocapture
/// ```
#[test]
#[ignore = "the long fuzz variant: minutes, not assertions (see module docs)"]
fn fuzz_long_both_backends() {
    for seed in 0x1234_5678..0x1234_5678 + 16 {
        sweep(seed, 400);
    }
}

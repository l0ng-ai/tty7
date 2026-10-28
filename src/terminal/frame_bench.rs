//! Phase 2d latency measurement for the libghostty-vt migration
//! (docs/libghostty-migration.md): what one paint's grid build costs for a
//! 200×50 viewport of mixed-SGR cells, before and after the render path
//! moved onto the engine-neutral frame.
//!
//! `#[ignore]`d like `crates/tty59-cli/src/bench_vt.rs` — it reports, not
//! asserts — and meant for release:
//!
//! ```text
//! cargo test --release --bin tty59 frame_bench -- --ignored --nocapture
//! ```
//!
//! This file lives beside `element/` rather than in it because the "before"
//! arm is the pre-migration code itself, pinned to commit d921d75 and still
//! naming the engine — the one thing `element.rs` must not do anymore. The
//! "after" arm calls today's live `snapshot_cell` over a frame the facade
//! filled, so the comparison is exactly the delta the phase introduced: the
//! engine walk became an engine→model copy plus a model walk.
//!
//! What the workload leaves out, deliberately, on *both* arms so neither is
//! flattered: OSC 8 links and SGR 2 dim runs. Their paint cost lives in
//! `osc8_underline` and the legibility solve, which the migration did not
//! touch — only the type of the cell they read changed.

use std::hint::black_box;
use std::time::Instant;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::index::{Line as AlacLine, Point as AlacPoint, Column as AlacColumn};
use alacritty_terminal::term::cell::{Cell as AlacCell, Flags as AlacFlags};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor};
use tty59_vt::model::{Frame, GridPoint, Rgb};
use tty59_vt::VtTermView;

use super::element::{PaintColors, RenderCell, snapshot_cell};
use super::element::tests::test_colors;

const COLS: usize = 200;
const ROWS: usize = 50;

/// The mixed-SGR screen both arms walk: bold/colored labels, truecolor,
/// inverse video, every underline style, wide CJK runs, and plain text —
/// rotating per row so no row's cells resolve the same way.
fn feed_workload() -> Term<VoidListener> {
    let mut term = Term::new(
        Config { scrolling_history: 10_000, ..Config::default() },
        &(BenchSize { cols: COLS, rows: ROWS }),
        VoidListener,
    );
    let mut parser: Processor = Processor::new();
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
    parser.advance(&mut term, &bytes);
    term
}

struct BenchSize {
    cols: usize,
    rows: usize,
}

impl alacritty_terminal::grid::Dimensions for BenchSize {
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

fn to_hsla(c: Rgb) -> gpui::Hsla {
    gpui::Rgba {
        r: c.r as f32 / 255.,
        g: c.g as f32 / 255.,
        b: c.b as f32 / 255.,
        a: 1.,
    }
    .into()
}

// ---- The before arm: element.rs at d921d75, verbatim except that it
// writes this bench's own cell struct (element's RenderCell is private to
// element, and this arm must not reach back into it) and drops the two
// branches the workload leaves out (above). -------------------------------

#[derive(Clone, Copy, PartialEq, Default)]
enum LegacyUnderline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// Written every iteration, read by no one: the writes themselves are the
/// work being timed, and the `black_box` over the finished buffers keeps
/// them from being optimized away.
#[expect(dead_code, reason = "a bench artifact, not a value")]
#[derive(Clone, Default)]
struct LegacyCell {
    c: char,
    marks: Option<Box<[char]>>,
    fg: gpui::Hsla,
    bg: gpui::Hsla,
    draw_bg: bool,
    bold: bool,
    italic: bool,
    strikeout: bool,
    underline: LegacyUnderline,
    underline_color: Option<gpui::Hsla>,
    spacer: bool,
    selected: bool,
}

fn legacy_resolve(
    color: AnsiColor,
    palette: &[Rgb; 256],
    default_fg: Rgb,
    default_bg: Rgb,
) -> (Rgb, bool) {
    match color {
        AnsiColor::Spec(rgb) => (Rgb::from(rgb), false),
        AnsiColor::Indexed(i) => (palette[i as usize], false),
        AnsiColor::Named(named) => match named {
            NamedColor::Foreground => (default_fg, true),
            NamedColor::Background => (default_bg, true),
            other => {
                let idx = other as usize;
                if idx < 256 {
                    (palette[idx], false)
                } else {
                    (default_fg, true)
                }
            }
        },
    }
}

fn legacy_snapshot_cell(
    cell: &AlacCell,
    _point: AlacPoint,
    palette: &[Rgb; 256],
    colors: &PaintColors,
) -> LegacyCell {
    let flags = cell.flags;
    if flags.contains(AlacFlags::WIDE_CHAR_SPACER) || flags.contains(AlacFlags::LEADING_WIDE_CHAR_SPACER) {
        return LegacyCell {
            spacer: true,
            ..LegacyCell::default()
        };
    }

    let inverse = flags.contains(AlacFlags::INVERSE);
    let (mut fgc, _) = legacy_resolve(cell.fg, palette, colors.fg_rgb, colors.bg_rgb);
    let (bgc, bg_default) = legacy_resolve(cell.bg, palette, colors.fg_rgb, colors.bg_rgb);
    let (fgc, bgc, draw_bg) = if inverse {
        (bgc, fgc, true)
    } else {
        if flags.contains(AlacFlags::HIDDEN) {
            fgc = bgc;
        }
        (fgc, bgc, !bg_default)
    };

    LegacyCell {
        c: cell.c,
        marks: cell
            .zerowidth()
            .filter(|marks| !marks.is_empty())
            .map(Box::from),
        fg: to_hsla(fgc),
        bg: to_hsla(bgc),
        draw_bg,
        bold: flags.contains(AlacFlags::BOLD) || flags.contains(AlacFlags::BOLD_ITALIC),
        italic: flags.contains(AlacFlags::ITALIC) || flags.contains(AlacFlags::BOLD_ITALIC),
        strikeout: flags.contains(AlacFlags::STRIKEOUT),
        underline: if flags.contains(AlacFlags::DOUBLE_UNDERLINE) {
            LegacyUnderline::Double
        } else if flags.contains(AlacFlags::UNDERCURL) {
            LegacyUnderline::Curly
        } else if flags.contains(AlacFlags::DOTTED_UNDERLINE) {
            LegacyUnderline::Dotted
        } else if flags.contains(AlacFlags::DASHED_UNDERLINE) {
            LegacyUnderline::Dashed
        } else if flags.contains(AlacFlags::UNDERLINE) {
            LegacyUnderline::Single
        } else {
            LegacyUnderline::None
        },
        underline_color: cell
            .underline_color()
            .map(|c| to_hsla(legacy_resolve(c, palette, colors.fg_rgb, colors.bg_rgb).0)),
        ..LegacyCell::default()
    }
    // SGR 2 dim, OSC 8 and the selection check: see the module docs — not
    // in the workload, so not in either arm.
}

/// The direct engine walk `build_grid` did before the frame: one pass over
/// `renderable_content`'s display iterator, cell by cell into the buffer.
fn run_legacy(
    term: &Term<VoidListener>,
    palette: &[Rgb; 256],
    colors: &PaintColors,
    buf: &mut Vec<LegacyCell>,
) {
    buf.clear();
    buf.resize(ROWS * COLS, LegacyCell::default());
    let content = term.renderable_content();
    let display_offset = content.display_offset as i32;
    for cell in content.display_iter {
        let row = cell.point.line.0 + display_offset;
        let col = cell.point.column.0;
        if row < 0 || row as usize >= ROWS || col >= COLS {
            continue;
        }
        let rc = legacy_snapshot_cell(cell.cell, cell.point, palette, colors);
        buf[row as usize * COLS + col] = rc;
    }
}

/// The frame path: the facade copies the viewport into model rows under the
/// lock, then the paint walks those rows into render cells.
fn run_framed(
    view: &VtTermView<'_>,
    palette: &[Rgb; 256],
    colors: &PaintColors,
    frame: &mut Frame,
    buf: &mut Vec<RenderCell>,
) {
    buf.clear();
    buf.resize(ROWS * COLS, RenderCell::default());
    view.frame(frame, false);
    let display_offset = frame.display_offset as i32;
    for (row, cells) in frame.rows.iter().enumerate() {
        if row >= ROWS {
            break;
        }
        for (col, cell) in cells.cells.iter().enumerate() {
            if col >= COLS {
                break;
            }
            let point = GridPoint {
                line: row as i32 - display_offset,
                col: col as u16,
            };
            buf[row * COLS + col] = snapshot_cell(cell, point, palette, colors, None);
        }
    }
}

#[test]
#[ignore = "latency measurement, not an assertion (see module docs)"]
fn bench_frame_build_200x50() {
    let term = feed_workload();
    let mut palette = [Rgb { r: 0, g: 0, b: 0 }; 256];
    for (i, slot) in palette.iter_mut().enumerate() {
        slot.r = i as u8;
        slot.g = (i * 3 % 256) as u8;
        slot.b = (i * 7 % 256) as u8;
    }
    let colors = test_colors();

    // Sanity: the frame the facade filled holds the same cells the legacy
    // arm walks directly off the engine — same character, same unresolved
    // color — or the two timings below would be of different work.
    {
        let mut frame = Frame::default();
        VtTermView::new(&term).frame(&mut frame, false);
        let probe = |row: i32, col: usize| {
            let engine = &term.grid()[AlacLine(row)][AlacColumn(col)];
            let model = &frame.rows[row as usize].cells[col];
            (
                engine.c,
                tty59_vt::model::StyleColor::from(engine.fg),
                model.c,
                model.fg,
            )
        };
        let (ec, efg, mc, mfg) = probe(0, 0);
        assert_eq!(ec, mc, "the styled label cell crosses");
        assert_eq!(efg, mfg, "with its unresolved color");
        let (ec, _, mc, _) = probe(2, 100);
        assert_eq!(ec, mc, "the inverse row cell crosses");
    }

    let mut legacy_buf = Vec::new();
    let mut frame = Frame::default();
    let mut buf = Vec::new();
    let view = VtTermView::new(&term);

    // Warm both arms (allocator, branch predictors, the frame's first fill).
    for _ in 0..10 {
        run_legacy(&term, &palette, &colors, &mut legacy_buf);
        run_framed(&view, &palette, &colors, &mut frame, &mut buf);
    }

    // Interleave the arms one round at a time so whatever the machine is
    // doing in the background (another build, a browser tab) hits both
    // equally, and take medians — a mean would let one stalled round bury
    // the signal on this fine-grained a workload.
    let rounds = 300;
    let mut legacy_times = Vec::with_capacity(rounds);
    let mut framed_times = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t0 = Instant::now();
        run_legacy(&term, &palette, &colors, &mut legacy_buf);
        let t1 = Instant::now();
        run_framed(&view, &palette, &colors, &mut frame, &mut buf);
        let t2 = Instant::now();
        legacy_times.push(t1 - t0);
        framed_times.push(t2 - t1);
    }

    black_box(&legacy_buf);
    black_box(&buf);
    black_box(&frame);

    let median = |times: &mut Vec<std::time::Duration>| {
        times.sort();
        times[times.len() / 2].as_secs_f64() * 1000.
    };
    let (legacy_ms, framed_ms) = (median(&mut legacy_times), median(&mut framed_times));
    eprintln!(
        "frame build 200×50, median of {rounds} interleaved rounds: \
         before (direct engine walk) {:.3} ms; after (facade frame + model \
         walk) {:.3} ms; after/before {:.2}×",
        legacy_ms,
        framed_ms,
        framed_ms / legacy_ms,
    );
}

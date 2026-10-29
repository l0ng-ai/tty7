//! The differential harness (docs/libghostty-migration.md, Phase 3): the
//! same byte streams into both backends, split at random chunk sizes, and
//! the results compared — normalized frames, cursor, modes, and the ordered
//! event streams.
//!
//! Run with the Ghostty backend compiled in:
//!
//! ```text
//! cargo test -p tty59-vt --features ghostty --test diff
//! ```
//!
//! ## The allow-list
//!
//! Known, intended differences are filtered *by name, with a reason*, before
//! comparison — never by loosening an assert:
//!
//! - **XTVERSION answered.** The Ghostty backend reports `tty59 <version>`;
//!   the engine being replaced never answered `CSI > q`. Filter: DCS `>|`
//!   replies, Ghostty side only.
//! - **DA2/DA3 answered with the wrapper's numbers.** DA1 is pinned equal
//!   (`ESC[?6c`, the daemon's own answer); DA2's firmware number is the
//!   reporting build's, and DA3 is answered where the old engine said
//!   nothing. Filter: DA2/DA3 reply shapes, both sides.
//! - **Kitty keyboard always spoken.** The replaced engine spoke it only
//!   when configured; Ghostty has no off. Filter: `CSI ? u` query replies
//!   and the `KITTY_*` mode bits.
//! - **Grapheme clustering (DEC mode 2027).** On by design in the Ghostty
//!   backend, so a regional-indicator pair is one wide cell there and two
//!   narrow cells on the other side. The frame comparison joins wide pairs
//!   into their text, which makes the two spellings of the same glyphs
//!   equal; style vectors are compared on ASCII rows only, where cell
//!   boundaries cannot shift.
//! - **ED2 does not push the screen into scrollback.** History depth is not
//!   compared (the viewport is); the tui workload's rewritten rows are.
//! - **OSC 52 reads double-answer.** The Ghostty engine emits its empty
//!   clipboard reply itself and the embedder's real answer follows later.
//!   Filter: empty `OSC 52` paste replies, Ghostty side only.
//! - **A tab is pen movement, not a cell.** Text extraction reads both as
//!   spaces-to-tab-stop on the seam, so row text compares equal.
//! - **SGR 53 overline.** The replaced engine never parsed it; the Ghostty
//!   one overlines. The style comparison masks the one flag.
//! - **Color queries answer from configured options.** OSC 4/10/11/12 `?`
//!   replies come from the engine on the Ghostty side (the theme pushed
//!   through `VtOptions`) and from the embedder on the other
//!   (`ColorRequest`). Path only; DA1 and DSR replies compare byte-equal.

#![cfg(feature = "ghostty")]

use std::sync::{Arc, Mutex};

use tty59_vt::model::{
    CellFlags, CursorShape, Frame, GridPoint, Modes, Scroll, SearchQuery, SelectionKind, Side,
    StyleColor, Underline, VtOptions,
};
use tty59_vt::{AlacTerm, VtEvent, VtEventListener, VtTerm};

/// Collects both engines' events in stream order.
#[derive(Clone, Default)]
struct Tap(Arc<Mutex<Vec<VtEvent>>>);

impl VtEventListener for Tap {
    fn send_event(&self, event: VtEvent) {
        self.0.lock().unwrap().push(event);
    }
}

/// One corpus entry: a name and its bytes.
struct Case {
    name: &'static str,
    bytes: Vec<u8>,
    cols: u16,
    rows: u16,
}

/// The bench workloads' shapes (crates/tty59-cli/src/bench_vt.rs), small
/// enough to compare in milliseconds: plain wrapped lines, and cursor-
/// addressed color frames.
fn case_plaintext() -> Case {
    const WORDS: &[&str] = &[
        "to", "be", "or", "not", "to", "be", "that", "is", "the", "question", "whether", "tis",
        "nobler", "in", "the", "mind", "to", "suffer", "the", "slings",
    ];
    let mut out = Vec::new();
    let mut line = String::new();
    let mut state = 0x5EED_u64;
    let mut word = 0usize;
    while out.len() < 40_000 {
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
    Case { name: "plaintext", bytes: out, cols: 80, rows: 24 }
}

fn case_tui_frames() -> Case {
    let mut out = Vec::new();
    for frame in 0..40u32 {
        out.extend_from_slice(b"\x1b[H\x1b[2J");
        for row in 0..24u32 {
            out.extend_from_slice(
                format!("\x1b[{};1H\x1b[48;5;{}m", row + 1, (frame + row) % 256).as_bytes(),
            );
            out.extend(std::iter::repeat_n(b' ', 68));
            out.extend_from_slice(format!("f{:06}", frame).as_bytes());
            out.extend_from_slice(b"\x1b[0m");
        }
    }
    Case { name: "tui frames", bytes: out, cols: 80, rows: 24 }
}

/// The hand-written streams: the shapes a terminal meets that the bulk
/// workloads do not.
fn cases() -> Vec<Case> {
    let mut v = vec![
        Case {
            name: "sgr",
            bytes: b"\x1b[1;31mbold red\x1b[0m \x1b[3;4mitalic underlined\x1b[0m\r\n\
                    \x1b[38;2;10;20;30mtruecolor\x1b[0m \x1b[48;5;99m bg 99\x1b[0m\r\n\
                    \x1b[7minverse\x1b[0m \x1b[2mfaint\x1b[0m \x1b[9mstrike\x1b[0m\r\n\
                    \x1b[4:3mcurly\x1b[0m \x1b[4:4mdotted\x1b[0m \x1b[53moverline\x1b[0m"
                .to_vec(),
            cols: 40,
            rows: 6,
        },
        Case {
            name: "wide and emoji",
            // Wide runs kept unstyled, styled runs kept ASCII: cell
            // boundaries cannot shift inside either (see the allow-list).
            bytes: "CJK: \u{4f60}\u{597d}\u{8bed}\u{8a00}\r\n\
                    emoji: \u{2764}\u{fe0f}\u{2764}\u{fe0f} end\r\n\
                    marks: e\u{301}\u{301}cole\r\n\
                    \x1b[32mgreen ascii\x1b[0m"
                .as_bytes()
                .to_vec(),
            cols: 40,
            rows: 6,
        },
        Case {
            name: "variation selectors",
            // VS15 (FE0E, text presentation) leaves its base one column;
            // VS16 (FE0F, emoji presentation) reserves two — the fork's
            // patch #2, and the grapheme-cluster rule the new engine
            // applies. The cursor's landing after each pair is the
            // reservation, and the per-chunk comparison pins it.
            bytes: "text \u{2764}\u{fe0e} one\r\n\
                    emoji \u{2764}\u{fe0f} two\r\n\
                    edge:\u{2764}\u{fe0f}\r\n\
                    pair \u{2764}\u{fe0e}\u{2764}\u{fe0f} end"
                .as_bytes()
                .to_vec(),
            cols: 40,
            rows: 6,
        },
        Case {
            name: "wide wrap and fills",
            // A wide character pushed to the next row by the wrap, and a
            // row filled only by an erase's background: the shapes where
            // text extraction's spellings disagreed before Phase 6a moved
            // it onto the engine's bulk formatter.
            bytes: "ab\u{4f60}\u{597d}\u{8bed}\r\n\x1b[44m\x1b[K\r\ntail\r\n"
                .as_bytes()
                .to_vec(),
            cols: 7,
            rows: 6,
        },
        Case {
            name: "alt screen",
            bytes: b"before\r\n\x1b[?1049h\x1b[2J\x1b[1;1Hfull screen\r\nprogram\r\n\x1b[?1049lafter"
                .to_vec(),
            cols: 30,
            rows: 5,
        },
        Case {
            name: "scroll region",
            bytes: b"one\r\ntwo\r\nthree\r\n\x1b[2;3r\x1b[3;1Hinside\r\n\x1b[r\x1b[4;1Hbelow"
                .to_vec(),
            cols: 20,
            rows: 5,
        },
        Case {
            name: "decscusr",
            bytes: b"\x1b[2 qblock\x1b[5 qbeam\r\n\x1b[3 qunderline\x1b[0 qdefault"
                .to_vec(),
            cols: 20,
            rows: 4,
        },
        Case {
            name: "osc",
            bytes: b"\x1b]0;the title\x07\x1b]2;other\x07\x1b]7;file:///home/t\x07\x07text\x1b]8;;https://e.com\x07link\x1b]8;;\x07 end"
                .to_vec(),
            cols: 40,
            rows: 4,
        },
        Case {
            name: "wraps",
            bytes: b"abcdefghij\r\nkl\r\n\r\nspaces    end\r\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
                .to_vec(),
            cols: 10,
            rows: 5,
        },
        Case {
            name: "tabs",
            bytes: b"a\tb\tc\r\nlong\tline\twith\tstops\x1b[5Gjump"
                .to_vec(),
            cols: 16,
            rows: 4,
        },
        Case {
            name: "colors query",
            bytes: b"\x1b[c\x1b[>c\x1b]11;?\x07\x1b[6n"
                .to_vec(),
            cols: 20,
            rows: 4,
        },
    ];
    v.push(case_plaintext());
    v.push(case_tui_frames());

    // The recorded corpus (Phase 3b): real tools at a fixed 80×24 — vim,
    // nvim, htop, less, and tmux itself rendering inside the pane — plus
    // the hand-written agent-TUI stream (synchronized output, alt screen,
    // bracketed paste) for the shape no recorder here produces.
    let recorded: &[(&str, &[u8])] = &[
        ("vim", include_bytes!("fixtures/vim.raw")),
        ("nvim", include_bytes!("fixtures/nvim.raw")),
        ("htop", include_bytes!("fixtures/htop.raw")),
        ("less", include_bytes!("fixtures/less.raw")),
        ("tmux", include_bytes!("fixtures/tmux.raw")),
        ("agent tui", include_bytes!("fixtures/agent-tui.raw")),
    ];
    for (name, bytes) in recorded {
        v.push(Case { name, bytes: bytes.to_vec(), cols: 80, rows: 24 });
    }
    v
}

// ---- Normalization ----------------------------------------------------------

/// What a frame is compared as: per row, the text (wide pairs joined,
/// spacers dropped, trailing padding trimmed) and — when the row is pure
/// ASCII, where cell boundaries cannot shift — the per-cell styles.
#[derive(Debug, PartialEq)]
struct NormalFrame {
    rows: Vec<NormalRow>,
    cursor: Option<(u16, u16, bool, bool)>,
    alt_screen: bool,
    display_offset: usize,
}

#[derive(Debug, PartialEq)]
struct NormalRow {
    text: String,
    /// (fg, bg, flags, underline) per non-spacer cell, ASCII rows only.
    styles: Option<Vec<(StyleColor, StyleColor, CellFlags, Underline)>>,
}

fn normal_frame(frame: &Frame) -> NormalFrame {
    let rows = frame
        .rows
        .iter()
        .map(|row| {
            let ascii = row.cells.iter().all(|c| c.c.is_ascii() || c.c == ' ');
            let mut text = String::new();
            let mut styles = Vec::new();
            for cell in &row.cells {
                match cell.wide {
                    tty59_vt::model::CellWidth::SpacerTail
                    | tty59_vt::model::CellWidth::SpacerHead => continue,
                    _ => {
                        // A tab is a literal cell on the replaced engine and
                        // pen movement on the new one (allow-list): the
                        // columns line up either way, so the one character
                        // is normalized to the space it renders as.
                        text.push(if cell.c == '\t' { ' ' } else { cell.c });
                        text.extend(cell.marks.iter().copied());
                        if ascii {
                            styles.push((
                                cell.fg,
                                cell.bg,
                                // SGR 53 overline is masked: the replaced
                                // engine never parsed it, the Ghostty one
                                // does — a capability add on the new side,
                                // not a behavior change to paper over.
                                CellFlags::from(cell) & !CellFlags::OVERLINE,
                                cell.underline,
                            ));
                        }
                    }
                }
            }
            NormalRow { text: text.trim_end().to_string(), styles: ascii.then_some(styles) }
        })
        .collect();
    NormalFrame {
        rows,
        cursor: frame.cursor.map(|c| (c.x, c.y, c.visible, c.shape == CursorShape::HollowBlock)),
        alt_screen: frame.alt_screen,
        display_offset: frame.display_offset,
    }
}

/// The mode bits both engines agree to be compared: the kitty protocol's
/// are masked (always spoken on one side, configured on the other), and
/// the three mouse-reporting modes collapse to the union bit the model
/// itself defines — the engines model tracking differently (independent
/// bits vs one enum where a superset mode reports its subsets as set,
/// which is xterm's reading), and "is the program taking the mouse" is
/// the question every consumer asks through `MOUSE_MODE`.
fn comparable_modes(modes: Modes) -> Modes {
    let mouse_union = if modes.intersects(Modes::MOUSE_MODE) {
        Modes::MOUSE_MODE
    } else {
        Modes::empty()
    };
    ((modes & !Modes::MOUSE_REPORT_CLICK & !Modes::MOUSE_DRAG & !Modes::MOUSE_MOTION)
        | mouse_union)
        & !Modes::KITTY_DISAMBIGUATE
        & !Modes::KITTY_REPORT_ALL_KEYS
        & !Modes::KITTY_REPORT_TEXT
        & !Modes::KITTY_REPORT_EVENT_TYPES
        & !Modes::KITTY_REPORT_ALTERNATE_KEYS
}

/// One event stream, with the allow-listed differences filtered out. Every
/// filter names its reason; nothing is dropped silently.
fn comparable_events(events: Vec<VtEvent>) -> Vec<VtEvent> {
    events
        .into_iter()
        .filter(|event| match event {
            // XTVERSION: the Ghostty backend answers `tty59 <version>`,
            // the replaced engine nothing (allow-list).
            VtEvent::PtyWrite(b) if b.starts_with(b"\x1bP>|") => false,
            // DA2's firmware number is the reporting build's; DA3 is
            // answered at all on the Ghostty side only (allow-list).
            VtEvent::PtyWrite(b)
                if b.starts_with(b"\x1b[>0;") && b.ends_with(b"c") =>
            {
                false
            }
            VtEvent::PtyWrite(b) if b.starts_with(b"\x1bP!|") => false,
            // The kitty flags query: answered where the protocol is spoken
            // (allow-list — one engine has it configured on, the other
            // always on). The reply carries whatever flags are in effect,
            // so the whole `CSI ? N u` reply shape is filtered.
            VtEvent::PtyWrite(b)
                if b.starts_with(b"\x1b[?")
                    && b.ends_with(b"u")
                    && b[3..b.len() - 1].iter().all(u8::is_ascii_digit) =>
            {
                false
            }
            // XTWINOPS and mode-2048 size reports: the alacritty side asks
            // the embedder (`TextAreaSizeRequest`), the Ghostty one answers
            // from the geometry its resizes carried. Reply path only, and
            // the GUI's own answer is pinned by its tests.
            VtEvent::TextAreaSizeRequest(_) => false,
            VtEvent::PtyWrite(b)
                if b.starts_with(b"\x1b[52;")
                    || b.starts_with(b"\x1b[4;")
                    || b.starts_with(b"\x1b[6;")
                    || b.starts_with(b"\x1b[8;")
                    || b.starts_with(b"\x1b[9;") =>
            {
                false
            }
            // DECRQM echoes (`CSI ? N ; V $ y`): the value an engine
            // reports for a mode is its own support and defaults — 2027 is
            // set by design on the Ghostty side, and 69/2031 defaults
            // differ. The live mode bits themselves are compared strictly
            // at every chunk (see `comparable_modes`); only the echo of
            // asking is filtered here.
            VtEvent::PtyWrite(b)
                if b.starts_with(b"\x1b[?")
                    && b.ends_with(b"$y")
                    && b.contains(&b';') =>
            {
                false
            }
            // Title restore on alt-screen exit: the replaced engine puts
            // the pre-app title back when a full-screen program leaves,
            // Ghostty keeps whatever the app left. Phase 4 parity item
            // (shells re-set their own titles at the next prompt, so the
            // GUI impact is one prompt's worth of stale tab text).
            VtEvent::ResetTitle => false,
            // OSC 52 read: the Ghostty engine emits its empty reply ahead
            // of the embedder's real one (allow-list).
            VtEvent::PtyWrite(b) if b == b"\x1b]52;c;\x07" => false,
            // OSC 7 pwd: the Ghostty engine reports it; tty59's own OSC
            // tokenizer stays authoritative for the cwd (decision 7) and
            // the GUI ignores this event — informational only.
            VtEvent::PwdChanged(_) => false,
            // Dynamic color queries (OSC 4/10/11/12 `?`): the alacritty
            // side asks the embedder (`ColorRequest`), the Ghostty one
            // answers itself from the theme colors configured through
            // `VtOptions` — the reply *path* is the difference, and the
            // reply *bytes* are pinned separately (the theme-change test).
            VtEvent::ColorRequest(..) => false,
            VtEvent::PtyWrite(b)
                if b.starts_with(b"\x1b]10;rgb:")
                    || b.starts_with(b"\x1b]11;rgb:")
                    || b.starts_with(b"\x1b]12;rgb:")
                    || b.starts_with(b"\x1b]4;") =>
            {
                false
            }
            _ => true,
        })
        .collect()
}

// ---- The driver ---------------------------------------------------------------

/// A deterministic splitter: the same stream, the same cut points, for both
/// engines. Simple xorshift — reproducibility is the point, not quality.
struct Chunks<'a> {
    bytes: &'a [u8],
    at: usize,
    state: u64,
}

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a [u8];
    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.bytes.len() {
            return None;
        }
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        let len = (self.state % 48 + 1) as usize;
        let end = (self.at + len).min(self.bytes.len());
        let chunk = &self.bytes[self.at..end];
        self.at = end;
        Some(chunk)
    }
}

fn both_engines(case: &Case, tap_a: &Tap, tap_g: &Tap) -> (AlacTerm<Tap>, VtTerm<Tap>) {
    let options = VtOptions::default();
    let alac = AlacTerm::with_listener(options.clone(), case.cols, case.rows, tap_a.clone());
    let ghost = VtTerm::with_listener(options, case.cols, case.rows, tap_g.clone());
    (alac, ghost)
}

/// Feed one chunk to both engines, then compare everything the seam can
/// read. The first mismatch returns a rendered diff of what disagreed.
fn step_and_compare(
    name: &str,
    fed: usize,
    alac: &mut AlacTerm<Tap>,
    ghost: &mut VtTerm<Tap>,
) -> Result<(), String> {
    let mut frame_a = Frame::default();
    let mut frame_g = Frame::default();
    alac.view().frame(&mut frame_a, false);
    ghost.view().frame(&mut frame_g, false);
    let (na, ng) = (normal_frame(&frame_a), normal_frame(&frame_g));
    if na != ng {
        for (row, (a, g)) in na.rows.iter().zip(&ng.rows).enumerate() {
            if a != g {
                return Err(format!(
                    "{name}: after {fed} bytes, row {row} differs:\n  alacritty: {a:?}\n  ghostty:  {g:?}"
                ));
            }
        }
        if na.rows.len() != ng.rows.len() {
            return Err(format!(
                "{name}: after {fed} bytes, row counts differ: {} != {}",
                na.rows.len(),
                ng.rows.len()
            ));
        }
        if na.cursor != ng.cursor {
            return Err(format!(
                "{name}: after {fed} bytes, the cursor differs: {:?} != {:?}",
                na.cursor, ng.cursor
            ));
        }
        if na.alt_screen != ng.alt_screen || na.display_offset != ng.display_offset {
            return Err(format!(
                "{name}: after {fed} bytes, alt/display differ: ({}, {}) != ({}, {})",
                na.alt_screen, na.display_offset, ng.alt_screen, ng.display_offset
            ));
        }
        return Err(format!(
            "{name}: after {fed} bytes, the frame differs: {na:?} != {ng:?}"
        ));
    }

    // Inside a synchronized-output hold the engines legitimately differ
    // in *live* state: the replaced one buffers the bytes until the hold
    // lifts, the new one parses them and freezes only the rendered frame
    // (compared equal above). The hold is short; comparing resumes after.
    if alac.sync_timeout().is_some() || ghost.sync_timeout().is_some() {
        return Ok(());
    }
    let cursor_a = alac.view().cursor();
    let cursor_g = ghost.view().cursor();
    if (cursor_a.x, cursor_a.y, cursor_a.visible, cursor_a.screen)
        != (cursor_g.x, cursor_g.y, cursor_g.visible, cursor_g.screen)
    {
        return Err(format!(
            "{name}: after {fed} bytes, the cursor differs: {cursor_a:?} != {cursor_g:?}"
        ));
    }

    let modes_a = comparable_modes(alac.view().modes());
    let modes_g = comparable_modes(ghost.view().modes());
    if modes_a != modes_g {
        return Err(format!(
            "{name}: after {fed} bytes, the modes differ: {modes_a:?} != {modes_g:?}"
        ));
    }
    Ok(())
}

#[test]
fn both_backends_agree_on_the_corpus() {
    for case in cases() {
        // One tap per engine: the events stay attributable, and each
        // stream is compared in its own order.
        let (tap_a, tap_g) = (Tap::default(), Tap::default());
        let (mut alac, mut ghost) = both_engines(&case, &tap_a, &tap_g);

        let mut fed = 0usize;
        for chunk in (Chunks { bytes: &case.bytes, at: 0, state: 0x9E37_79B9 }) {
            alac.feed(chunk);
            ghost.feed(chunk);
            fed += chunk.len();
            if let Err(e) = step_and_compare(case.name, fed, &mut alac, &mut ghost) {
                panic!("{e}");
            }
        }

        // Text extraction over the whole grid, scrollback included. Tabs
        // are the allow-listed difference (Phase 0 finding): one engine
        // stores a literal `\t`, the other moves the pen — the extracted
        // spacing differs by a column here and there while the content
        // never does, and reconstructing tabs is a Phase 4 decision. So the
        // comparison collapses whitespace runs: content-equal, tolerant of
        // the spelling, and still strict about every other character.
        let collapse_ws = |text: String| {
            let mut out = String::with_capacity(text.len());
            let mut spaces = 0usize;
            for c in text.chars() {
                match c {
                    '\t' | ' ' => spaces += 1,
                    '\n' => {
                        spaces = 0;
                        out.push('\n');
                    }
                    _ => {
                        if spaces > 0 {
                            out.push(' ');
                            spaces = 0;
                        }
                        out.push(c);
                    }
                }
            }
            out
        };
        // Only the live screen's rows compare: ED 2 does not push the
        // screen into scrollback on the Ghostty side (allow-list), so a
        // frame-churning stream leaves history on one engine and none on
        // the other — the viewport, which the per-chunk frame comparison
        // already covers strictly, is the shared fact.
        let live_rows = |text: String| -> Vec<String> {
            let lines: Vec<String> = collapse_ws(text).split('\n').map(str::to_owned).collect();
            lines[lines.len().saturating_sub(case.rows as usize)..].to_vec()
        };
        let text_a = live_rows(alac.text_full());
        let text_g = live_rows(ghost.text_full());
        if text_a != text_g {
            panic!(
                "{name}: the live screen's text differs\n  alacritty: {text_a:?}\n  ghostty:  {text_g:?}",
                name = case.name
            );
        }

        // The ordered event streams, allow-listed differences filtered.
        let a_events = comparable_events(tap_a.0.lock().unwrap().clone());
        let g_events = comparable_events(tap_g.0.lock().unwrap().clone());
        if a_events != g_events {
            panic!(
                "{name}: the event streams differ\n  alacritty: {a_events:?}\n  ghostty:  {g_events:?}",
                name = case.name
            );
        }
    }
}

/// Selections behave the same through the seam on both engines: a span's
/// text and the copy gate's questions.
#[test]
fn both_backends_agree_on_selections() {
    let case = Case {
        name: "selection",
        bytes: b"one two three\r\nsecond line here\r\nthird".to_vec(),
        cols: 20,
        rows: 4,
    };
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let (mut alac, mut ghost) = both_engines(&case, &tap_a, &tap_g);
    alac.feed(&case.bytes);
    ghost.feed(&case.bytes);

    let probe = |term: &mut AlacTerm<Tap>| -> (bool, bool, Option<String>) {
        let mut session = term.session();
        session.start_selection(
            SelectionKind::Simple,
            GridPoint { line: 0, col: 4 },
            Side::Left,
        );
        let gated = session.has_selection();
        session.update_selection(GridPoint { line: 1, col: 9 }, Side::Right);
        (gated, session.has_selection(), session.selection_to_string())
    };
    let a = probe(&mut alac);

    let probe_ghost = |term: &mut VtTerm<Tap>| -> (bool, bool, Option<String>) {
        let mut session = term.session();
        session.start_selection(
            SelectionKind::Simple,
            GridPoint { line: 0, col: 4 },
            Side::Left,
        );
        let gated = session.has_selection();
        session.update_selection(GridPoint { line: 1, col: 9 }, Side::Right);
        (gated, session.has_selection(), session.selection_to_string())
    };
    let g = probe_ghost(&mut ghost);
    assert_eq!(a, g, "the selection gesture reads the same on both engines");
    assert_eq!(a.2.as_deref(), Some("two three\nsecond lin"));
}

/// Search finds the same matches on both: plain (search.h vs the replaced
/// engine's walker), regex, and case-sensitive (the text matcher vs the
/// walker) — Phase 3b closed the gap the Unsupported status stood for.
#[test]
fn both_backends_agree_on_every_search_shape() {
    let bytes = b"hello World\r\nplain world row\r\nWORLD end";
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let (mut alac, mut ghost) = both_engines(
        &Case { name: "search", bytes: bytes.to_vec(), cols: 24, rows: 4 },
        &tap_a,
        &tap_g,
    );
    alac.feed(bytes);
    ghost.feed(bytes);

    for (regex, case_sensitive, text) in [
        (false, false, "world"),
        (true, false, "wor.d"),
        (false, true, "world"),
        (true, true, "W.rld"),
        (false, false, "\u{2014}missing\u{2014}"),
    ] {
        let query = SearchQuery {
            text: text.to_string(),
            regex,
            case_sensitive,
        };
        let mut search_a = tty59_vt::VtSearch::new();
        search_a.set_query(query.clone());
        search_a.feed(&mut alac.session());
        let mut search_g = tty59_vt::VtSearch::new();
        search_g.set_query(query);
        search_g.feed(&mut ghost.session());
        assert_eq!(
            (search_a.matches(), search_a.status()),
            (search_g.matches(), search_g.status()),
            "regex={regex} case={case_sensitive} {text:?}"
        );
    }
    // And the counts the shapes promise, on both.
    let run = |term: &mut VtTerm<Tap>, regex: bool, case: bool, text: &str| {
        let mut search = tty59_vt::VtSearch::new();
        search.set_query(SearchQuery {
            text: text.to_string(),
            regex,
            case_sensitive: case,
        });
        search.feed(&mut term.session());
        search.count()
    };
    assert_eq!(run(&mut ghost, false, false, "world"), 3);
    assert_eq!(run(&mut ghost, false, true, "world"), 1);
    assert_eq!(run(&mut ghost, true, false, "wor.d"), 3);
}

/// The selection kinds behave the same through the seam: a word gesture
/// expands to the separators on both engines, a line gesture spans whole
/// rows, and a block cuts its rectangle.
#[test]
fn both_backends_agree_on_selection_kinds() {
    let bytes = b"one two three\r\nsecond line here\r\nthird row text";
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let (mut alac, mut ghost) = both_engines(
        &Case { name: "selection kinds", bytes: bytes.to_vec(), cols: 20, rows: 4 },
        &tap_a,
        &tap_g,
    );
    alac.feed(bytes);
    ghost.feed(bytes);

    // Word: click mid-word on row 0, drag to mid-word on row 1 — the span
    // is whole words at both ends, on both engines.
    let mut a = alac.session();
    a.start_selection(
        SelectionKind::Semantic,
        GridPoint { line: 0, col: 6 },
        Side::Left,
    );
    a.update_selection(GridPoint { line: 1, col: 8 }, Side::Right);
    let mut g = ghost.session();
    g.start_selection(
        SelectionKind::Semantic,
        GridPoint { line: 0, col: 6 },
        Side::Left,
    );
    g.update_selection(GridPoint { line: 1, col: 8 }, Side::Right);
    assert_eq!(
        (a.selection(), a.selection_to_string()),
        (g.selection(), g.selection_to_string()),
        "a word-wise drag reads the same on both engines"
    );
    let expected = a.selection().expect("a word selection");
    assert_eq!(expected.span.start, GridPoint { line: 0, col: 4 }, "the word's left edge");
    assert_eq!(expected.span.end, GridPoint { line: 1, col: 10 }, "through 'line'");

    // Line: whole rows, trailing newline.
    let mut a = alac.session();
    a.set_selection(
        SelectionKind::Lines,
        GridPoint { line: 0, col: 5 },
        Side::Left,
        GridPoint { line: 1, col: 2 },
        Side::Right,
    );
    let mut g = ghost.session();
    g.set_selection(
        SelectionKind::Lines,
        GridPoint { line: 0, col: 5 },
        Side::Left,
        GridPoint { line: 1, col: 2 },
        Side::Right,
    );
    assert_eq!(
        (a.selection(), a.selection_to_string()),
        (g.selection(), g.selection_to_string()),
        "a line selection reads the same on both engines"
    );
    assert_eq!(
        a.selection_to_string().as_deref(),
        Some("one two three\nsecond line here\n")
    );

    // Block: the rectangle's kind crosses and cuts every row at the same
    // columns.
    let mut a = alac.session();
    a.set_selection(
        SelectionKind::Block,
        GridPoint { line: 0, col: 4 },
        Side::Left,
        GridPoint { line: 2, col: 7 },
        Side::Right,
    );
    let mut g = ghost.session();
    g.set_selection(
        SelectionKind::Block,
        GridPoint { line: 0, col: 4 },
        Side::Left,
        GridPoint { line: 2, col: 7 },
        Side::Right,
    );
    assert_eq!(
        (a.selection(), a.selection_to_string()),
        (g.selection(), g.selection_to_string()),
        "a block selection reads the same on both engines"
    );
}

/// OSC 8 grouping: adjacent cells with the same URI are one link (one
/// side-table index), a different URI is another — the URI-per-cell rule
/// both engines report, which the hover walk and the underline run key on.
#[test]
fn both_backends_agree_on_hyperlink_grouping() {
    let bytes = b"a \x1b]8;;https://e.com\x07run one\x1b]8;;\x07 b \x1b]8;;https://e.com\x07run two\x1b]8;;\x07 c \x1b]8;;https://f.io\x07third\x1b]8;;\x07 end";
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let (mut alac, mut ghost) = both_engines(
        &Case { name: "links", bytes: bytes.to_vec(), cols: 60, rows: 3 },
        &tap_a,
        &tap_g,
    );
    alac.feed(bytes);
    ghost.feed(bytes);

    // The URIs per cell read the same through the seam's walker view.
    for col in 0..60u16 {
        let a = alac.view().glyph(GridPoint { line: 0, col }).hyperlink;
        let g = ghost.view().glyph(GridPoint { line: 0, col }).hyperlink;
        assert_eq!(a, g, "col {col} carries the same link on both engines");
    }

    // And the frame interns them the same: one index per URI, runs share.
    let mut frame_a = Frame::default();
    let mut frame_g = Frame::default();
    alac.view().frame(&mut frame_a, false);
    ghost.view().frame(&mut frame_g, false);
    let links = |f: &Frame| -> Vec<Option<u16>> {
        f.rows[0].cells.iter().map(|c| c.hyperlink).collect()
    };
    assert_eq!(links(&frame_a), links(&frame_g));
    assert_eq!(frame_a.hyperlinks, frame_g.hyperlinks);
    assert_eq!(frame_a.hyperlinks.len(), 2, "two distinct URIs, two indexes");
}

/// `clear` and `^L` at an integrated prompt keep the screen in history on
/// both engines: tty59's shell integration emits OSC 133, the bytes reach
/// each engine whole, and Ghostty's own at-prompt heuristic turns ED2 into
/// a scroll-clear where the replaced engine scrolls unconditionally — the
/// same user-visible outcome (scroll up after `clear`, the output is
/// still there) by each engine's native rule.
#[test]
fn clear_at_an_integrated_prompt_keeps_history_on_both() {
    let prompt = b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\";
    let mut bytes = Vec::new();
    bytes.extend_from_slice(prompt);
    bytes.extend_from_slice(b"echo hi\r\noutput line one\r\noutput line two\r\n");
    bytes.extend_from_slice(prompt);
    bytes.extend_from_slice(b"\x1b[H\x1b[2J");
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let (mut alac, mut ghost) = both_engines(
        &Case { name: "clear at prompt", bytes: bytes.clone(), cols: 20, rows: 5 },
        &tap_a,
        &tap_g,
    );
    alac.feed(&bytes);
    ghost.feed(&bytes);

    assert_eq!(
        alac.view().history_size(),
        ghost.view().history_size(),
        "the same number of rows landed in history"
    );
    assert!(ghost.view().history_size() >= 3, "the output survived the clear");
    let text = ghost.text_full();
    assert!(text.contains("output line two"), "scroll-up after clear still reads it");
    assert!(alac.text_full().contains("output line two"));
}

/// The ED2 parity rewrite, end to end through the engine: a clear with no
/// shell integration (an SSH session's remote shell) keeps history on
/// both engines, split-proof, and never fires inside a string payload or
/// on a near-miss sequence.
#[test]
fn both_backends_agree_on_ed2_history() {
    // The ssh-like shape: output, no OSC 133 anywhere, then `clear`.
    let ssh_like = b"remote shell output\r\nmore output\r\nlast line\r\n\x1b[H\x1b[2J";
    for cut in [0, 5, 21, 34, ssh_like.len() - 1] {
        let (tap_a, tap_g) = (Tap::default(), Tap::default());
        let (mut alac, mut ghost) = both_engines(
            &Case { name: "ed2", bytes: ssh_like.to_vec(), cols: 20, rows: 5 },
            &tap_a,
            &tap_g,
        );
        let (head, tail) = ssh_like.split_at(cut);
        alac.feed(head);
        ghost.feed(head);
        alac.feed(tail);
        ghost.feed(tail);
        assert_eq!(
            (alac.view().history_size(), ghost.view().history_size()),
            (3, 3),
            "cut at {cut}: the clear kept the same history on both engines"
        );
        assert!(ghost.text_full().contains("last line"), "scroll-up after clear reads it");
    }
}

/// The ED2 rewrite never fires where it must not: inside an OSC title,
/// inside an APC frame, on `CSI 1;2 J` / `CSI ? 2 J`, or on the alternate
/// screen — the sequences reach the engine verbatim and do their own
/// (non-scrolling) work on both sides.
#[test]
fn the_ed2_rewrite_leaves_payloads_and_near_misses_alone() {
    // An OSC title carrying ED2 bytes. A raw ESC inside an OSC is not
    // well-formed, and the engines each handle the abort their own way —
    // alacritty cancels the string and parses the CSI as a real ED2 (it
    // scrolls twice), Ghostty cancels and drops it — a pre-existing
    // engine difference, not the rewriter's. The rewriter's contract here
    // is that it did NOT rewrite inside the payload: Ghostty scrolls
    // exactly once (the real ED2 after the title), which is the count a
    // rewrite-inside-payload would have doubled.
    let title = b"\x1b]0;fake \x1b[2J title\x07after\x1b[2J";
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let (mut alac, mut ghost) = both_engines(
        &Case { name: "ed2 in title", bytes: title.to_vec(), cols: 20, rows: 5 },
        &tap_a,
        &tap_g,
    );
    alac.feed(b"row\r\n");
    ghost.feed(b"row\r\n");
    alac.feed(title);
    ghost.feed(title);
    assert_eq!(ghost.view().history_size(), 2, "one scroll: the real ED2 only");
    assert!(ghost.text_full().contains("after"), "the post-title text landed");

    // Near misses: `CSI 1;2 J` and `CSI ? 2 J` do not scroll on ghostty
    // (no rewrite), and their text lands the same on both.
    let near = b"keep\r\nrows\r\n\x1b[1;2J\x1b[?2J";
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let (mut alac, mut ghost) = both_engines(
        &Case { name: "ed2 near miss", bytes: near.to_vec(), cols: 20, rows: 5 },
        &tap_a,
        &tap_g,
    );
    alac.feed(near);
    ghost.feed(near);
    assert_eq!(
        ghost.view().history_size(), 0,
        "no rewrite fired: the near-misses cleared without scrolling"
    );

    // An APC frame carrying the bytes: payload is payload.
    let apc = b"\x1b_Ga=T;\x1b[2J\x1b\\tail\r\n";
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let (mut alac, mut ghost) = both_engines(
        &Case { name: "ed2 in apc", bytes: apc.to_vec(), cols: 20, rows: 5 },
        &tap_a,
        &tap_g,
    );
    alac.feed(apc);
    ghost.feed(apc);
    assert_eq!(ghost.view().history_size(), 0, "the APC payload passed through");

    // The alternate screen: ED2 there clears in place on both engines.
    let alt = b"\x1b[?1049h\x1b[H\x1b[2J";
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let (mut alac, mut ghost) = both_engines(
        &Case { name: "ed2 on alt", bytes: alt.to_vec(), cols: 20, rows: 5 },
        &tap_a,
        &tap_g,
    );
    alac.feed(alt);
    ghost.feed(alt);
    assert!(ghost.alt());
    assert_eq!(ghost.view().history_size(), 0, "no scroll off the primary screen");
}

/// `clear` as shells send it (`ESC[H ESC[2J ESC[3J`): the 3J wipes the
/// scrollback the rewrite just filled, so history ends empty on both
/// engines — the rewrite must never resurrect what 3J deleted.
#[test]
fn the_full_clear_sequence_ends_with_empty_history_on_both() {
    let bytes = b"line one\r\nline two\r\nline three\r\n\x1b[H\x1b[2J\x1b[3J";
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let (mut alac, mut ghost) = both_engines(
        &Case { name: "clear 3J", bytes: bytes.to_vec(), cols: 20, rows: 5 },
        &tap_a,
        &tap_g,
    );
    alac.feed(bytes);
    ghost.feed(bytes);
    assert_eq!(alac.view().history_size(), 0, "alacritty: 3J took it all");
    assert_eq!(ghost.view().history_size(), 0, "ghostty: the rewrite scrolled, 3J wiped");
}

/// A selection that reaches into scrollback survives streaming output the
/// same way on both engines — the rows it names keep meaning what they
/// meant, or the selection goes with them, identically.
#[test]
fn both_backends_agree_on_selections_across_streaming() {
    let mut bytes = Vec::new();
    for i in 0..14 {
        bytes.extend_from_slice(format!("row {i:02}\r\n").as_bytes());
    }
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let (mut alac, mut ghost) = both_engines(
        &Case { name: "selection streaming", bytes: bytes.clone(), cols: 20, rows: 5 },
        &tap_a,
        &tap_g,
    );
    alac.feed(&bytes);
    ghost.feed(&bytes);

    // Select from scrollback into the live screen, then stream more rows
    // past the scrollback cap (the options default to 10k lines, so this
    // stays under it: the drift here is the grid moving under the span,
    // not pruning — pruning is the fuzz's territory).
    let mut a = alac.session();
    a.set_selection(
        SelectionKind::Simple,
        GridPoint { line: -3, col: 0 },
        Side::Left,
        GridPoint { line: 1, col: 4 },
        Side::Right,
    );
    let mut g = ghost.session();
    g.set_selection(
        SelectionKind::Simple,
        GridPoint { line: -3, col: 0 },
        Side::Left,
        GridPoint { line: 1, col: 4 },
        Side::Right,
    );
    let more = b"row 14\r\nrow 15\r\nrow 16\r\n";
    alac.feed(more);
    ghost.feed(more);

    assert_eq!(
        (alac.view().selection(), alac.view().selection_to_string()),
        (ghost.view().selection(), ghost.view().selection_to_string()),
        "a span into scrollback reads the same after streaming"
    );
    // The span followed its rows: the text still names whole rows.
    let text = alac.view().selection_to_string().unwrap_or_default();
    assert!(
        text.lines().next().is_some_and(|l| l.starts_with("row 0")),
        "the scrollback end kept its text: {text:?}"
    );
}

/// Scroll anchoring while output streams: a viewport scrolled into history
/// stays put (anchored) as new rows arrive, and the live screen's viewport
/// follows the output — the two behaviors the scrollbar and the follow
/// logic are built on. Pinned engine against engine.
#[test]
fn both_backends_agree_on_scroll_anchoring_while_streaming() {
    let mut start = Vec::new();
    for i in 0..14 {
        start.extend_from_slice(format!("row {i:02}\r\n").as_bytes());
    }
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let (mut alac, mut ghost) = both_engines(
        &Case { name: "anchoring", bytes: start.clone(), cols: 20, rows: 5 },
        &tap_a,
        &tap_g,
    );
    alac.feed(&start);
    ghost.feed(&start);

    // Scrolled into history: three rows up. Streaming more output must not
    // yank the viewport back down (the user is reading), and the same rows
    // must still be showing — the anchor the seam promises.
    alac.session().scroll(Scroll::Delta(3));
    ghost.session().scroll(Scroll::Delta(3));
    let mut more = Vec::new();
    for i in 14..24 {
        more.extend_from_slice(format!("row {i:02}\r\n").as_bytes());
    }
    alac.feed(&more);
    ghost.feed(&more);

    assert_eq!(
        alac.view().viewport_offset(),
        ghost.view().viewport_offset(),
        "the anchored viewport moved the same"
    );
    let mut frame_a = Frame::default();
    let mut frame_g = Frame::default();
    alac.view().frame(&mut frame_a, false);
    ghost.view().frame(&mut frame_g, false);
    let text = |f: &Frame| {
        f.rows[0]
            .cells
            .iter()
            .map(|c| c.c)
            .collect::<String>()
            .trim_end()
            .to_string()
    };
    assert_eq!(text(&frame_a), text(&frame_g), "the same row is on top");
    assert!(text(&frame_a).starts_with("row 0"), "still the history it anchored to");

    // Back at the live screen: the viewport follows output again, and the
    // newest row is the one showing at the bottom.
    alac.session().scroll(Scroll::Bottom);
    ghost.session().scroll(Scroll::Bottom);
    let tail = b"row 24\r\n";
    alac.feed(tail);
    ghost.feed(tail);
    assert_eq!(
        alac.view().viewport_offset(),
        ghost.view().viewport_offset(),
        "both followed the output back to the live screen"
    );
}

/// Scrolling by the model's targets lands the viewport in the same place.
#[test]
fn both_backends_agree_on_scrolling() {
    let mut bytes = Vec::new();
    for i in 0..30 {
        bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
    }
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let (mut alac, mut ghost) = both_engines(
        &Case { name: "scroll", bytes: bytes.clone(), cols: 20, rows: 5 },
        &tap_a,
        &tap_g,
    );
    alac.feed(&bytes);
    ghost.feed(&bytes);

    for delta in [3, -1, 100, -100] {
        alac.session().scroll(Scroll::Delta(delta));
        ghost.session().scroll(Scroll::Delta(delta));
        assert_eq!(
            alac.view().viewport_offset(),
            ghost.view().viewport_offset(),
            "Delta({delta}) lands the same"
        );
    }
    alac.session().scroll(Scroll::Top);
    ghost.session().scroll(Scroll::Top);
    assert_eq!(alac.view().viewport_offset(), ghost.view().viewport_offset());
}

/// ConPTY resize (fork patch #3, carried to the new engine through
/// `RESIZE_PULL_SCROLLBACK`), on the shapes a real ConPTY paints and both
/// engines agree on: growing opens blank rows at the bottom without pulling
/// scrollback, and shrinking anchors the last written row — the PSReadLine
/// shape, with painted rows below the cursor. conhost's post-resize paints
/// are absolute-row, so both grids and cursors must land identically.
/// (The cursor-high shrink, where the fork protects the cursor and the new
/// engine bottom-anchors like conhost, is pinned engine-side instead —
/// see the Ghostty backend's `conpty_shrink_bottom_anchors_when_the_
/// cursor_sits_high`.)
#[test]
fn both_backends_agree_on_conpty_resizes() {
    let options = || VtOptions {
        conpty_resize: true,
        scrollback_lines: 10,
        ..VtOptions::default()
    };

    // Grow: [1, 2] with "1" scrolled into history becomes [2, 3, blank,
    // blank], the cursor still on its row, the history untouched.
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let mut alac = AlacTerm::with_listener(options(), 2, 2, tap_a);
    let mut ghost = VtTerm::with_listener(options(), 2, 2, tap_g);
    let grow = b"1\r\n2\r\n3";
    alac.feed(grow);
    ghost.feed(grow);
    alac.session().resize(2, 4);
    ghost.session().resize(2, 4);
    assert_eq!(
        (alac.view().history_size(), ghost.view().history_size()),
        (1, 1),
        "grow: the scrolled-out row stays in history on both — never pulled back"
    );
    assert!(step_and_compare("conpty grow", grow.len(), &mut alac, &mut ghost).is_ok());

    // Shrink, the PSReadLine shape: the cursor on "3" with "4" painted
    // below it; the last written row anchors to the new bottom and the
    // cursor follows its row up.
    let (tap_a, tap_g) = (Tap::default(), Tap::default());
    let mut alac = AlacTerm::with_listener(options(), 2, 4, tap_a);
    let mut ghost = VtTerm::with_listener(options(), 2, 4, tap_g);
    let shrink = b"1\r\n2\r\n3\r\n4";
    alac.feed(shrink);
    ghost.feed(shrink);
    alac.session().set_cursor(GridPoint { line: 2, col: 0 }, false);
    ghost.session().set_cursor(GridPoint { line: 2, col: 0 }, false);
    alac.session().resize(2, 2);
    ghost.session().resize(2, 2);
    assert_eq!(
        (alac.view().history_size(), ghost.view().history_size()),
        (2, 2),
        "shrink: the rows above went to history on both"
    );
    let err = step_and_compare("conpty shrink", shrink.len(), &mut alac, &mut ghost);
    assert!(err.is_ok(), "{err:?}");
}




//! The corpus and the comparison machinery the differential harness and
//! the golden runner share (Phase 6b): every case, the normalization that
//! makes two engines' answers comparable, the deterministic chunker, and
//! the per-step parity comparison.
//!
//! The differential harness (diff.rs) drives every case through **both**
//! engines and proves they agree; the golden runner (golden.rs) records
//! the Ghostty engine's answers as reviewable text files, generated only
//! while the parity proof passes — so a golden is a frozen, once-checked
//! oracle for the engine that remains after the alacritty backend's
//! deletion.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use tty59_vt::model::{
    CellFlags, CursorShape, Frame, Modes, StyleColor, Underline, VtOptions,
};
use tty59_vt::{AlacTerm, VtEvent, VtEventListener, VtTerm};

/// The event collector both engines report into.
#[derive(Default, Clone)]
pub struct Tap(pub Arc<Mutex<Vec<VtEvent>>>);

impl VtEventListener for Tap {
    fn send_event(&self, event: VtEvent) {
        self.0.lock().unwrap().push(event);
    }
}

/// One corpus entry: a name, its bytes, its grid — and the two extra
/// gestures a case may need, a ConPTY resize mode and one resize step
/// after the bytes land.
pub struct Case {
    pub name: &'static str,
    pub bytes: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
    /// Resize with ConPTY row semantics (`VtOptions::conpty_resize`) on
    /// both engines.
    pub conpty: bool,
    /// Resize to this grid after feeding the bytes, before reading the
    /// answer.
    pub resize: Option<(u16, u16)>,
}

pub fn case(name: &'static str, bytes: Vec<u8>, cols: u16, rows: u16) -> Case {
    Case { name, bytes, cols, rows, conpty: false, resize: None }
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
    case("plaintext", out, 80, 24)
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
    case("tui frames", out, 80, 24)
}

/// The hand-written streams: the shapes a terminal meets that the bulk
/// workloads do not.
pub fn cases() -> Vec<Case> {
    let mut v = vec![
        case(
            "sgr",
            b"\x1b[1;31mbold red\x1b[0m \x1b[3;4mitalic underlined\x1b[0m\r\n\
              \x1b[38;2;10;20;30mtruecolor\x1b[0m \x1b[48;5;99m bg 99\x1b[0m\r\n\
              \x1b[7minverse\x1b[0m \x1b[2mfaint\x1b[0m \x1b[9mstrike\x1b[0m\r\n\
              \x1b[4:3mcurly\x1b[0m \x1b[4:4mdotted\x1b[0m \x1b[53moverline\x1b[0m"
                .to_vec(),
            40,
            6,
        ),
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
            conpty: false,
            resize: None,
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
            conpty: false,
            resize: None,
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
            conpty: false,
            resize: None,
        },
        case(
            "alt screen",
            b"before\r\n\x1b[?1049h\x1b[2J\x1b[1;1Hfull screen\r\nprogram\r\n\x1b[?1049lafter".to_vec(),
            30,
            5,
        ),
        case(
            "scroll region",
            b"one\r\ntwo\r\nthree\r\n\x1b[2;3r\x1b[3;1Hinside\r\n\x1b[r\x1b[4;1Hbelow".to_vec(),
            20,
            5,
        ),
        case(
            "decscusr",
            b"\x1b[2 qblock\x1b[5 qbeam\r\n\x1b[3 qunderline\x1b[0 qdefault".to_vec(),
            20,
            4,
        ),
        case(
            "osc",
            b"\x1b]0;the title\x07\x1b]2;other\x07\x1b]7;file:///home/t\x07\x07text\x1b]8;;https://e.com\x07link\x1b]8;;\x07 end".to_vec(),
            40,
            4,
        ),
        case(
            "wraps",
            b"abcdefghij\r\nkl\r\n\r\nspaces    end\r\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".to_vec(),
            10,
            4,
        ),
        // The ED2 family (Phase 4's parity rewriter): an SSH-like clear
        // with no integration marks keeps history; a clear at an
        // integrated prompt does too (natively, through OSC 133); the
        // near-misses and payload-carriers reach the engine verbatim; the
        // common `clear` sequence still ends with empty history.
        case(
            "ed2 ssh like",
            b"remote shell output\r\nmore output\r\nlast line\r\n\x1b[H\x1b[2J".to_vec(),
            20,
            5,
        ),
        case(
            "ed2 at prompt",
            b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\echo hi\r\noutput line one\r\noutput line two\r\n\
              \x1b]133;A\x1b\\$ \x1b]133;B\x1b\\\x1b[H\x1b[2J"
                .to_vec(),
            20,
            5,
        ),
        // The payload carriers and near misses that both engines spell
        // the same: a well-formed OSC title, a clean APC frame, a CUP'd
        // real ED2, and text after. (The shapes that
        // genuinely differ — a raw ESC aborting an OSC title, an ESC
        // carried inside an APC payload, the malformed multi-param
        // `CSI 1;2 J`, `CSI ?2 J`, `CSI 22 J` — stay in their dedicated
        // differential test, where the difference itself is the pin.)
        case(
            "ed2 payloads and near misses",
            b"\x1b]0;a clean title\x07after\x1b[H\x1b[2J\x1b_Ga=T,f=24,s=1,v=1;frame\x1b\\tail".to_vec(),
            30,
            4,
        ),
        case(
            "clear all",
            b"gone after this\r\n\x1b[H\x1b[2J\x1b[3J".to_vec(),
            20,
            5,
        ),
        // ConPTY resizes (Phase 5, fork patch #3): grow opens blank rows
        // with the cursor pinned; shrink anchors the last written row —
        // the PSReadLine shape, with painted rows below the cursor.
        Case {
            name: "conpty grow",
            bytes: b"1\r\n2\r\n3".to_vec(),
            cols: 2,
            rows: 2,
            conpty: true,
            resize: Some((2, 4)),
        },
        Case {
            name: "conpty shrink psreadline",
            bytes: b"1\r\n2\r\n3\r\n4\x1b[3;1H".to_vec(),
            cols: 2,
            rows: 4,
            conpty: true,
            resize: Some((2, 2)),
        },
    ];
    v.push(case_plaintext());
    v.push(case_tui_frames());

    // The recorded corpus (Phase 3b): real tools at a fixed 80×24 — vim,
    // nvim, htop, less, and tmux itself rendering inside the pane — plus
    // the hand-written agent-TUI stream (synchronized output, alt screen,
    // bracketed paste) for the shape no recorder here produces.
    let recorded: &[(&str, &[u8])] = &[
        ("vim", include_bytes!("../fixtures/vim.raw")),
        ("nvim", include_bytes!("../fixtures/nvim.raw")),
        ("htop", include_bytes!("../fixtures/htop.raw")),
        ("less", include_bytes!("../fixtures/less.raw")),
        ("tmux", include_bytes!("../fixtures/tmux.raw")),
        ("agent tui", include_bytes!("../fixtures/agent-tui.raw")),
    ];
    for (name, bytes) in recorded {
        v.push(case(name, bytes.to_vec(), 80, 24));
    }
    v
}

// ---- Normalization ----------------------------------------------------------

/// What a frame is compared as: per row, the text (wide pairs joined,
/// spacers dropped, trailing padding trimmed) and — when the row is pure
/// ASCII, where cell boundaries cannot shift — the per-cell styles.
#[derive(Debug, PartialEq)]
pub struct NormalFrame {
    pub rows: Vec<NormalRow>,
    pub cursor: Option<(u16, u16, bool, bool)>,
    pub alt_screen: bool,
    pub display_offset: usize,
}

#[derive(Debug, PartialEq)]
pub struct NormalRow {
    pub text: String,
    /// (fg, bg, flags, underline) per non-spacer cell, ASCII rows only.
    pub styles: Option<Vec<(StyleColor, StyleColor, CellFlags, Underline)>>,
}

pub fn normal_frame(frame: &Frame) -> NormalFrame {
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
pub fn comparable_modes(modes: Modes) -> Modes {
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
pub fn comparable_events(events: Vec<VtEvent>) -> Vec<VtEvent> {
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
pub struct Chunks<'a> {
    pub bytes: &'a [u8],
    pub at: usize,
    pub state: u64,
}

impl<'a> Chunks<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0, state: 0x9E37_79B9 }
    }
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

pub fn both_engines(case: &Case, tap_a: &Tap, tap_g: &Tap) -> (AlacTerm<Tap>, VtTerm<Tap>) {
    let options = VtOptions { conpty_resize: case.conpty, ..VtOptions::default() };
    let alac = AlacTerm::with_listener(options.clone(), case.cols, case.rows, tap_a.clone());
    let ghost = VtTerm::with_listener(options, case.cols, case.rows, tap_g.clone());
    (alac, ghost)
}

/// Feed one chunk to both engines, then compare everything the seam can
/// read. The first mismatch returns a rendered diff of what disagreed.
pub fn step_and_compare(
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
                na.rows.len(), ng.rows.len()
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

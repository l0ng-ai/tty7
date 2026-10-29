//! The golden runner (Phase 6b): every corpus case through the Ghostty
//! engine, its answer frozen as a reviewable text file under
//! `tests/golden/` — the oracle that survives the alacritty backend's
//! deletion, when the differential harness retires and this becomes the
//! corpus's guard.
//!
//! A golden is only ever **generated while the parity proof passes**: with
//! `GOLDEN_UPDATE=1`, each case first runs through both engines under the
//! differential harness's own per-chunk comparison (`common::step_and_
//! compare`, the named allow-list baked in), and only a case the engines
//! agree on writes its file. Every golden is therefore parity-checked
//! once, at the moment it is frozen.
//!
//! The recorded answer is the end state the seam can read: the normalized
//! frame (row text; per-cell colors and flags on pure-ASCII rows where
//! cell boundaries cannot shift), cursor, mode bits, display offset,
//! history depth, the comparable event stream, and a digest of the whole
//! grid's text. Reviewable text, not a binary format — a diff shows what a
//! behavior change actually changed.

#![cfg(feature = "ghostty")]

mod common;

use std::fmt::Write as _;
use std::path::PathBuf;

use common::*;
use tty59_vt::model::{Frame, Modes, VtOptions};
use tty59_vt::{VtEvent, VtTerm};

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn golden_path(name: &str) -> PathBuf {
    golden_dir().join(format!("{}.txt", name.replace(' ', "-")))
}

/// Everything one case's answer records, in the order the file spells it.
struct Answer {
    frame: NormalFrame,
    history: usize,
    modes: Modes,
    events: Vec<VtEvent>,
    text: String,
}

/// Drive one case exactly as the harness does — the same deterministic
/// chunks, the same resize step — and read the answer back.
fn ghost_answer(case: &Case, tap: &Tap) -> Answer {
    let options = VtOptions { conpty_resize: case.conpty, ..VtOptions::default() };
    let mut ghost = VtTerm::with_listener(options, case.cols, case.rows, tap.clone());
    for chunk in Chunks::new(&case.bytes) {
        ghost.feed(chunk);
    }
    if let Some((cols, rows)) = case.resize {
        ghost.session().resize(cols as usize, rows as usize);
    }
    let mut frame = Frame::default();
    ghost.view().frame(&mut frame, false);
    Answer {
        frame: normal_frame(&frame),
        history: ghost.view().history_size(),
        modes: comparable_modes(ghost.view().modes()),
        events: comparable_events(tap.0.lock().unwrap().clone()),
        text: ghost.text_full(),
    }
}

/// The golden file's exact spelling. Field by field, deterministic — the
/// runner compares strings, so any change in an engine's answer is a diff
/// in the file.
fn render(case: &Case, answer: &Answer) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "case: {}", case.name);
    let _ = writeln!(
        out,
        "grid: {}x{}{}{}",
        case.cols,
        case.rows,
        if case.conpty { " conpty" } else { "" },
        match case.resize {
            Some((c, r)) => format!(" resize-> {c}x{r}"),
            None => String::new(),
        }
    );
    let _ = writeln!(out, "history: {}", answer.history);
    let _ = writeln!(out, "display_offset: {}", answer.frame.display_offset);
    let _ = writeln!(out, "alt_screen: {}", answer.frame.alt_screen);
    let _ = writeln!(
        out,
        "cursor: {}",
        match answer.frame.cursor {
            Some((x, y, visible, hollow)) =>
                format!("{x},{y} visible={visible} hollow={hollow}"),
            None => "none".to_string(),
        }
    );
    let _ = writeln!(out, "modes: {:?}", answer.modes);
    let _ = writeln!(
        out,
        "text: chars={} sha256={}",
        answer.text.chars().count(),
        sha256_hex(answer.text.as_bytes()),
    );
    let head: String = answer.text.chars().take(96).collect();
    let tail: String = answer
        .text
        .chars()
        .rev()
        .take(96)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let _ = writeln!(out, "text head: {head:?}");
    let _ = writeln!(out, "text tail: {tail:?}");
    let _ = writeln!(out, "events: {}", answer.events.len());
    for event in &answer.events {
        let _ = writeln!(out, "  {event:?}");
    }
    let _ = writeln!(out, "frame rows: {}", answer.frame.rows.len());
    for (i, row) in answer.frame.rows.iter().enumerate() {
        let _ = writeln!(out, "  {i:4} | {}", row.text);
        if let Some(styles) = &row.styles {
            let _ = writeln!(out, "  {i:4} # {styles:?}");
        }
    }
    out
}

#[test]
fn goldens_hold() {
    let update = std::env::var_os("GOLDEN_UPDATE").is_some();
    for case in cases() {
        // Generation only ever happens under the parity proof: while both
        // engines exist, an update run first drives the case through the
        // differential comparison and refuses to freeze a file the engines
        // disagree on. (After the deletion this block goes; the runner
        // keeps replaying the frozen files.)
        if update {
            let (tap_a, tap_g) = (Tap::default(), Tap::default());
            let (mut alac, mut ghost) = both_engines(&case, &tap_a, &tap_g);
            let mut fed = 0usize;
            for chunk in Chunks::new(&case.bytes) {
                alac.feed(chunk);
                ghost.feed(chunk);
                fed += chunk.len();
                if let Err(e) = step_and_compare(case.name, fed, &mut alac, &mut ghost) {
                    panic!("refusing to freeze a golden: {e}");
                }
            }
            if let Some((cols, rows)) = case.resize {
                alac.session().resize(cols as usize, rows as usize);
                ghost.session().resize(cols as usize, rows as usize);
                if let Err(e) = step_and_compare(case.name, fed, &mut alac, &mut ghost) {
                    panic!("refusing to freeze a golden: {e}");
                }
            }
        }

        let tap = Tap::default();
        let answer = ghost_answer(&case, &tap);
        let rendered = render(&case, &answer);
        let path = golden_path(case.name);
        if update {
            std::fs::create_dir_all(golden_dir()).unwrap();
            std::fs::write(&path, &rendered).unwrap();
            eprintln!("froze {}", path.display());
            continue;
        }
        let frozen = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "cannot read the golden {} ({}). Regenerate with \
                 GOLDEN_UPDATE=1 while the differential harness still runs.",
                path.display(),
                e
            )
        });
        assert_eq!(
            frozen,
            rendered,
            "case {} drifted from its frozen golden (regenerate with \
             GOLDEN_UPDATE=1 after checking the drift is intentional)",
            case.name
        );
    }
}

/// SHA-256, self-contained (a golden's digest must not move when the
/// toolchain's dependencies move). Compact public-domain implementation.
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter().map(|w| format!("{w:08x}")).collect()
}

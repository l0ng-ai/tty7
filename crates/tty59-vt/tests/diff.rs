//! The differential harness: every corpus case through **both** engines,
//! compared at every deterministic chunk boundary — the parity proof that
//! froze the goldens (golden.rs) before the alacritty backend's deletion
//! (Phase 6b).
//!
//! The corpus, the normalization and the comparison live in `common`, so
//! the golden runner and this harness can never drift about what an
//! "answer" is. This file keeps only the two-engine assertions.

#![cfg(feature = "ghostty")]

mod common;

use common::*;
use tty59_vt::model::{
    Frame, GridPoint, Scroll, SearchQuery, SelectionKind, Side, VtOptions,
};
use tty59_vt::{AlacTerm, VtTerm};

#[test]
fn both_backends_agree_on_the_corpus() {
    for case in cases() {
        // One tap per engine: the events stay attributable, and each
        // stream is compared in its own order.
        let (tap_a, tap_g) = (Tap::default(), Tap::default());
        let (mut alac, mut ghost) = both_engines(&case, &tap_a, &tap_g);

        let mut fed = 0usize;
        for chunk in Chunks::new(&case.bytes) {
            alac.feed(chunk);
            ghost.feed(chunk);
            fed += chunk.len();
            if let Err(e) = step_and_compare(case.name, fed, &mut alac, &mut ghost) {
                panic!("{e}");
            }
        }
        // The cases with a resize step land it and compare again: the
        // ConPTY shapes through the engines' own resize paths.
        if let Some((cols, rows)) = case.resize {
            alac.session().resize(cols as usize, rows as usize);
            ghost.session().resize(cols as usize, rows as usize);
            if let Err(e) = step_and_compare(case.name, fed, &mut alac, &mut ghost) {
                panic!("{e}");
            }
        }

        // Text extraction over the whole grid, scrollback included. Tabs
        // are the allow-listed difference (Phase 0 finding): one engine
        // stores a literal `\t`, the other moves the pen — the extracted
        // spacing differs by a column here and there while the content
        // never does. So the comparison collapses whitespace runs:
        // content-equal, tolerant of the spelling, and still strict about
        // every other character.
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
    let case = case(
        "selection",
        b"one two three\r\nsecond line here\r\nthird".to_vec(),
        20,
        4,
    );
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
        &case("search", bytes.to_vec(), 24, 4),
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
        &case("selection kinds", bytes.to_vec(), 20, 4),
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
        &case("links", bytes.to_vec(), 60, 3),
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
        &case("clear at prompt", bytes.clone(), 20, 5),
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
            &case("ed2", ssh_like.to_vec(), 20, 5),
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
        &case("ed2 in title", title.to_vec(), 20, 5),
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
        &case("ed2 near miss", near.to_vec(), 20, 5),
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
        &case("ed2 in apc", apc.to_vec(), 20, 5),
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
        &case("ed2 on alt", alt.to_vec(), 20, 5),
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
        &case("clear 3J", bytes.to_vec(), 20, 5),
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
        &case("selection streaming", bytes.clone(), 20, 5),
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
        &case("anchoring", start.clone(), 20, 5),
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
        &case("scroll", bytes.clone(), 20, 5),
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

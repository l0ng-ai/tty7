//! Ghostty-side behavior pins that the migration's differential harness
//! carried as *divergence* pins — the shapes where the two engines
//! deliberately spelled things differently, asserted on this engine alone
//! (Phase 6b: the harness retired with the alacritty backend; these tests
//! and the goldens keep its knowledge).

mod common;

use common::Tap;
use tty59_vt::model::{Frame, GridPoint, Scroll, SelectionKind, Side, VtOptions};
use tty59_vt::{VtEvent, VtTerm};

fn term(cols: u16, rows: u16, bytes: &[u8]) -> VtTerm<Tap> {
    let tap = Tap::default();
    let mut term = VtTerm::with_listener(VtOptions::default(), cols, rows, tap.clone());
    term.feed(bytes);
    term
}

/// A raw ESC aborting an OSC title: the rewriter must not rewrite inside
/// the payload — the engine scrolls exactly once, on the real ED2 after
/// the title (a rewrite-inside-payload would double it). The replaced
/// engine parsed the aborted CSI as a second, real ED2 and scrolled twice;
/// that difference retired with it.
#[test]
fn an_ed2_inside_an_aborted_title_scrolls_once() {
    let title = b"\x1b]0;fake \x1b[2J title\x07after\x1b[2J";
    let mut term = term(20, 5, b"row\r\n");
    term.feed(title);
    assert_eq!(term.view().history_size(), 2, "one scroll: the real ED2 only");
    assert!(term.text_full().contains("after"), "the post-title text landed");
}

/// Near misses pass the rewriter by: `CSI 1;2 J` (multi-param) and
/// `CSI ?2 J` (DEC-private) are the engine's own business, no rewrite, no
/// scroll.
#[test]
fn ed2_near_misses_never_scroll() {
    let term = term(20, 5, b"keep\r\nrows\r\n\x1b[1;2J\x1b[?2J");
    assert_eq!(
        term.view().history_size(),
        0,
        "no rewrite fired: the near-misses cleared without scrolling"
    );
}

/// An APC frame carrying ED2 bytes: payload is payload, nothing executes,
/// nothing scrolls.
#[test]
fn an_ed2_inside_an_apc_frame_passes_through() {
    let term = term(20, 5, b"\x1b_Ga=T;\x1b[2J\x1b\\tail\r\n");
    assert_eq!(term.view().history_size(), 0, "the APC payload passed through");
    assert!(term.text_full().contains("tail"), "the text after the frame landed");
}

/// ED2 on the alternate screen clears in place — no scroll off the primary
/// screen's history.
#[test]
fn ed2_on_the_alt_screen_clears_in_place() {
    let term = term(20, 5, b"\x1b[?1049h\x1b[H\x1b[2J");
    assert!(term.alt());
    assert_eq!(term.view().history_size(), 0);
}

/// `clear` as shells send it (`ESC[H ESC[2J ESC[3J`): the rewrite scrolls
/// the screen into history and the 3J wipes it — history ends empty, the
/// rewrite must never resurrect what 3J deleted. Split-tolerant.
#[test]
fn the_full_clear_sequence_ends_with_empty_history() {
    let bytes = b"line one\r\nline two\r\nline three\r\n\x1b[H\x1b[2J\x1b[3J";
    for cut in [0, 9, 18, 27, bytes.len() - 1] {
        let (head, tail) = bytes.split_at(cut);
        let mut term = term(20, 5, head);
        term.feed(tail);
        assert_eq!(term.view().history_size(), 0, "cut at {cut}: 3J took it all");
    }
}

/// The ssh-like ED2 (no OSC 133 anywhere): the rewrite keeps history —
/// split-proof across the same cuts the harness drove.
#[test]
fn an_ed2_without_integration_marks_keeps_history() {
    let bytes = b"remote shell output\r\nmore output\r\nlast line\r\n\x1b[H\x1b[2J";
    for cut in [0, 5, 21, 34, bytes.len() - 1] {
        let (head, tail) = bytes.split_at(cut);
        let mut term = term(20, 5, head);
        term.feed(tail);
        assert_eq!(term.view().history_size(), 3, "cut at {cut}: the clear kept history");
        assert!(term.text_full().contains("last line"), "scroll-up after clear reads it");
    }
}

/// `clear` at an integrated prompt: the OSC 133 marks make the engine's
/// own at-prompt heuristic scroll — no rewrite needed, same outcome.
#[test]
fn a_clear_at_an_integrated_prompt_keeps_history() {
    let prompt = b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\";
    let mut bytes = Vec::new();
    bytes.extend_from_slice(prompt);
    bytes.extend_from_slice(b"echo hi\r\noutput line one\r\noutput line two\r\n");
    bytes.extend_from_slice(prompt);
    bytes.extend_from_slice(b"\x1b[H\x1b[2J");
    let term = term(20, 5, &bytes);
    assert!(term.view().history_size() >= 3, "the output survived the clear");
    assert!(
        term.text_full().contains("output line two"),
        "scroll-up after clear still reads it"
    );
}

/// Selection kinds: word gestures expand to the separators, line gestures
/// span whole rows, blocks cut their rectangle — the exact spans the
/// replaced engine agreed to.
#[test]
fn selection_kinds_span_words_lines_and_blocks() {
    let bytes = b"one two three\r\nsecond line here\r\nthird row text";
    let mut term = term(20, 4, bytes);
    {
        let mut session = term.session();
        session.start_selection(
            SelectionKind::Semantic,
            GridPoint { line: 0, col: 6 },
            Side::Left,
        );
        session.update_selection(GridPoint { line: 1, col: 8 }, Side::Right);
        let expected = session.selection().expect("a word selection");
        assert_eq!(expected.span.start, GridPoint { line: 0, col: 4 }, "the word's left edge");
        assert_eq!(expected.span.end, GridPoint { line: 1, col: 10 }, "through 'line'");
    }
    {
        let mut session = term.session();
        session.set_selection(
            SelectionKind::Lines,
            GridPoint { line: 0, col: 5 },
            Side::Left,
            GridPoint { line: 1, col: 2 },
            Side::Right,
        );
        assert_eq!(
            session.selection_to_string().as_deref(),
            Some("one two three\nsecond line here\n")
        );
    }
    {
        let mut session = term.session();
        session.set_selection(
            SelectionKind::Block,
            GridPoint { line: 0, col: 4 },
            Side::Left,
            GridPoint { line: 2, col: 7 },
            Side::Right,
        );
        let expected = session.selection().expect("a block selection");
        assert_eq!(expected.kind, SelectionKind::Block);
        assert_eq!(expected.span.start, GridPoint { line: 0, col: 4 });
        assert_eq!(expected.span.end, GridPoint { line: 2, col: 7 });
        let text = session.selection_to_string().unwrap_or_default();
        assert_eq!(
            text.split('\n').count(),
            3,
            "the rectangle cuts one line per row: {text:?}"
        );
    }
}

/// OSC 8 grouping: adjacent cells with the same URI are one link (one
/// side-table index), a different URI is another.
#[test]
fn hyperlinks_group_by_uri() {
    let bytes = b"a \x1b]8;;https://e.com\x07run one\x1b]8;;\x07 b \x1b]8;;https://e.com\x07run two\x1b]8;;\x07 c \x1b]8;;https://f.io\x07third\x1b]8;;\x07 end";
    let term = term(60, 3, bytes);
    // The URI per cell reads through the walker view.
    let on_e = term
        .view()
        .glyph(GridPoint { line: 0, col: 2 })
        .hyperlink;
    assert_eq!(on_e.as_deref(), Some("https://e.com"));
    // And the frame interns them: one index per URI, runs share.
    let mut frame = Frame::default();
    term.view().frame(&mut frame, false);
    assert_eq!(frame.hyperlinks.len(), 2, "two distinct URIs, two indexes");
}

/// A selection reaching into scrollback survives streaming output — the
/// rows it names keep meaning what they meant.
#[test]
fn a_selection_into_scrollback_survives_streaming() {
    let mut bytes = Vec::new();
    for i in 0..14 {
        bytes.extend_from_slice(format!("row {i:02}\r\n").as_bytes());
    }
    let mut term = term(20, 5, &bytes);
    term.session().set_selection(
        SelectionKind::Simple,
        GridPoint { line: -3, col: 0 },
        Side::Left,
        GridPoint { line: 1, col: 4 },
        Side::Right,
    );
    term.feed(b"row 14\r\nrow 15\r\nrow 16\r\n");
    let text = term.view().selection_to_string().unwrap_or_default();
    assert!(
        text.lines().next().is_some_and(|l| l.starts_with("row 0")),
        "the scrollback end kept its text: {text:?}"
    );
}

/// Scroll anchoring while output streams: a viewport scrolled into history
/// stays put as new rows arrive; back at the live screen it follows the
/// output again.
#[test]
fn a_scrolled_viewport_stays_anchored_while_streaming() {
    let mut start = Vec::new();
    for i in 0..14 {
        start.extend_from_slice(format!("row {i:02}\r\n").as_bytes());
    }
    let mut term = term(20, 5, &start);
    term.session().scroll(Scroll::Delta(3));
    let mut more = Vec::new();
    for i in 14..24 {
        more.extend_from_slice(format!("row {i:02}\r\n").as_bytes());
    }
    term.feed(&more);
    let mut frame = Frame::default();
    term.view().frame(&mut frame, false);
    let top: String = frame.rows[0].cells.iter().map(|c| c.c).collect();
    assert!(top.starts_with("row 0"), "still the history it anchored to: {top:?}");

    term.session().scroll(Scroll::Bottom);
    assert_eq!(term.view().viewport_offset(), 0, "back to the live screen");
}

/// Scrolling by the model's targets lands where it says.
#[test]
fn scrolling_targets_land() {
    let mut bytes = Vec::new();
    for i in 0..30 {
        bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
    }
    let mut term = term(20, 5, &bytes);
    for (delta, expected) in [(3, 3), (-1, 2), (100, 26), (-100, 0)] {
        term.session().scroll(Scroll::Delta(delta));
        assert_eq!(term.view().viewport_offset(), expected, "Delta({delta})");
    }
    term.session().scroll(Scroll::Top);
    assert_eq!(term.view().viewport_offset(), 26);
}

/// Search serves every query shape: plain through search.h, regex and
/// case-sensitive through the text matcher — with the counts the shapes
/// promise.
#[test]
fn search_serves_every_shape_with_counts() {
    let mut term = term(24, 4, b"hello World\r\nplain world row\r\nWORLD end");
    let run = |term: &mut VtTerm<Tap>, regex: bool, case: bool, text: &str| {
        let mut search = tty59_vt::VtSearch::new();
        search.set_query(tty59_vt::model::SearchQuery {
            text: text.to_string(),
            regex,
            case_sensitive: case,
        });
        search.feed(&mut term.session());
        search.count()
    };
    assert_eq!(run(&mut term, false, false, "world"), 3);
    assert_eq!(run(&mut term, false, true, "world"), 1);
    assert_eq!(run(&mut term, true, false, "wor.d"), 3);
    assert_eq!(run(&mut term, true, true, "W.rld"), 1);
}

/// Events the stream produces arrive in order (the comparable set the
/// golden records): a title, a bell, a pty reply.
#[test]
fn effects_arrive_in_order() {
    let tap = Tap::default();
    let mut term =
        VtTerm::with_listener(VtOptions::default(), 40, 4, tap.clone());
    term.feed(b"\x1b]2;the title\x07\x07text\x1b[6n");
    let events = tap.0.lock().unwrap().clone();
    assert!(events.contains(&VtEvent::Title("the title".into())));
    assert!(events.contains(&VtEvent::Bell));
    assert!(
        events.iter().any(|e| matches!(e, VtEvent::PtyWrite(b) if !b.is_empty())),
        "the DSR cursor query was answered: {events:?}"
    );
}

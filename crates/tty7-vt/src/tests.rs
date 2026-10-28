//! Wrapper unit tests. These run against the real pinned libghostty-vt
//! archive (via `TTY7_GHOSTTY_DIR`), so they double as the Phase 0 link
//! smoke test: if the crate builds and these pass, the static archive and
//! the generated bindings agree.

use crate::*;
use crate::model::{CellWidth, StyleColor};

fn term(cols: u16, rows: u16) -> Terminal {
    Terminal::new(cols, rows, 10_000).expect("terminal constructs")
}

/// A sink that panics on demand — every event, or just one kind.
struct PanicSink {
    only: Option<&'static str>,
}

impl EventSink for PanicSink {
    fn event(&mut self, event: VtEvent) {
        let name = match &event {
            VtEvent::PtyWrite(_) => "pty",
            VtEvent::Bell => "bell",
            VtEvent::TitleChanged(_) => "title",
            VtEvent::PwdChanged(_) => "pwd",
            VtEvent::DesktopNotification { .. } => "notify",
            VtEvent::ProgressReport { .. } => "progress",
        };
        if self.only.is_none() || self.only == Some(name) {
            panic!("sink panic under test");
        }
    }
}

#[test]
fn split_utf8_across_writes_lands_as_one_grapheme() {
    let mut t = term(20, 4);
    // é as UTF-8 is C3 A9; cut between the bytes.
    t.write("e".as_bytes());
    t.write(&[0xCC]);
    t.write(&[0x81]);
    t.write(b"cole");
    let snap = t.snapshot();
    let text: String = snap.rows[0].cells.iter().map(|c| c.text.as_str()).collect();
    // NFD on purpose: the stream spelled the accent as U+0301 and the grid
    // keeps it that way — matching what the alacritty path reproduces.
    assert_eq!(text, "e\u{301}cole");
    // One cell per grapheme: the combining mark joined the base cell
    // instead of taking a column of its own.
    assert_eq!(snap.rows[0].cells[0].text, "e\u{301}");
    assert_eq!(snap.rows[0].cells[0].text.chars().count(), 2);
    assert_eq!(snap.cursor.x, 5);
}

#[test]
fn split_escape_sequence_across_writes_still_styles() {
    let mut t = term(20, 4);
    // Bold and red, with the SGR cut mid-sequence: `1;3` + `1m` = SGR 1;31.
    t.write(b"\x1b[");
    t.write(b"1;3");
    t.write(b"1mhi");
    let snap = t.snapshot();
    assert_eq!(&snap.rows[0].cells[0].text, "h");
    assert!(snap.rows[0].cells[0].bold, "SGR 1 applied");
    assert_eq!(
        snap.rows[0].cells[0].fg,
        StyleColor::Palette(1),
        "SGR 31 applied"
    );
    assert!(!snap.rows[0].cells[0].italic, "no SGR 3 was sent");
}

#[test]
fn empty_write_is_a_no_op() {
    let mut t = term(10, 3);
    t.write(&[]);
    t.write(b"");
    let snap = t.snapshot();
    assert_eq!(snap.cursor, Cursor {
        x: 0, y: 0, visible: true, pending_wrap: false, screen: Screen::Primary,
    });
    assert!(snap.rows.iter().all(|r| r.cells.iter().all(|c| c.text.is_empty())));
}

#[test]
fn resize_down_to_one_by_one_keeps_the_grid_usable() {
    let mut t = term(40, 10);
    t.write(b"hello world, quite a long line here\r\nsecond\r\n");
    t.resize(1, 1, 8, 16).expect("minimum resize succeeds");
    assert_eq!(t.size(), (1, 1));
    // The terminal still parses after the squeeze.
    t.write(b"ok");
    assert_eq!(t.cursor().x, 0, "wrapped into the single column");
    assert!(t.text_full().contains('o'));
}

#[test]
fn vs16_emoji_occupies_two_columns() {
    let mut t = term(10, 3);
    // ❤️ = U+2764 U+FE0F: the variation selector widens the heart to two
    // columns (upstream fixed in 1.4.0; this guards the fork patch it
    // replaces). Write it after an ASCII anchor.
    t.write("a\u{2764}\u{fe0f}!".as_bytes());
    let snap = t.snapshot();
    let row = &snap.rows[0].cells;
    assert_eq!(&row[0].text, "a");
    assert_eq!(row[1].wide, CellWidth::Wide);
    assert_eq!(row[1].text, "\u{2764}\u{fe0f}");
    assert_eq!(row[2].wide, CellWidth::SpacerTail);
    assert!(row[2].text.is_empty(), "the spacer carries no text");
    assert_eq!(&row[3].text, "!");
    assert_eq!(snap.cursor.x, 4, "cursor advanced past both columns");
}

#[test]
fn five_thousand_kitty_mode_pushes_do_not_kill_the_parser() {
    // The alacritty fork patch #1 guard: overflowing the keyboard-mode
    // stack used to panic the reader after 4097 unpopped `CSI > 1 u`
    // pushes. Ghostty is unaffected, and this proves the wrapper path
    // survives the same flood.
    let mut t = term(80, 24);
    let flood = "\x1b[>1u".repeat(5000);
    t.write(flood.as_bytes());
    t.write(b"still alive\r\n");
    assert!(t.text_full().contains("still alive"));
}

#[test]
fn a_panicking_sink_does_not_abort_or_poison_the_terminal() {
    let mut t = term(20, 4);
    t.write_with(b"before \x07 after", &mut PanicSink { only: Some("bell") });
    assert!(t.callback_panicked(), "the panic was caught, not escaped");
    // The terminal kept parsing through the panic: the text after the BEL
    // landed.
    assert!(t.text_full().contains("before"));
    assert!(t.text_full().contains("after"));

    // And a later write with a healthy sink still works.
    let mut log = EventLog::default();
    t.write_with(b"\x07", &mut log);
    assert_eq!(log.events, vec![VtEvent::Bell]);
}

#[test]
fn effects_arrive_as_owned_events() {
    let mut t = term(40, 4);
    let mut log = EventLog::default();
    // Title (OSC 2), pwd (OSC 7), bell, a DSR cursor-position query (which
    // the terminal answers through write_pty), and a progress report.
    t.write_with(
        b"\x1b]2;the title\x07\x1b]7;file:///home/t\x07\x07\x1b[6n\x1b]9;4;1;50\x07",
        &mut log,
    );
    assert!(log.events.contains(&VtEvent::TitleChanged("the title".into())));
    assert!(log.events.contains(&VtEvent::PwdChanged("file:///home/t".into())));
    assert!(log.events.contains(&VtEvent::Bell));
    assert!(log.events.iter().any(|e| matches!(e, VtEvent::PtyWrite(bytes) if !bytes.is_empty())));
    assert!(log
        .events
        .contains(&VtEvent::ProgressReport { state: ProgressState::Set, progress: Some(50) }));
}

#[test]
fn text_full_joins_wrapped_rows_and_breaks_real_lines() {
    let mut t = term(10, 6);
    t.write("x".repeat(30).as_bytes());
    t.write(b"\r\nnext\r\nline\r\n");
    let text = t.text_full();
    assert_eq!(text, format!("{}\nnext\nline\n", "x".repeat(30)));
}

#[test]
fn text_full_includes_scrollback() {
    let mut t = term(10, 3);
    // Six rows of output through a three-row viewport: the first rows must
    // survive in scrollback, not vanish.
    t.write(b"r1\r\nr2\r\nr3\r\nr4\r\nr5\r\nr6\r\n");
    let text = t.text_full();
    assert!(text.contains("r1"));
    assert!(text.contains("r6"));
    let (total, scrollback) = t.scrollback();
    assert!(total >= 6);
    assert!(scrollback >= 3, "rows pushed off the viewport are history");
}

#[test]
fn snapshot_reflects_scrolling_and_returns_to_the_bottom() {
    let mut t = term(10, 3);
    t.write(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n");
    t.scroll(ScrollTarget::Top);
    assert!(!t.viewport_active(), "scrolled into history");
    let top: String = t
        .snapshot()
        .rows[0]
        .cells
        .iter()
        .map(|c| c.text.as_str())
        .collect();
    assert!(top.trim().is_empty() || top.starts_with("one"), "top of history first: {top:?}");
    t.scroll(ScrollTarget::Bottom);
    assert!(t.viewport_active());
    let all: String = t
        .snapshot()
        .rows
        .iter()
        .flat_map(|r| r.cells.iter().map(|c| c.text.as_str()))
        .collect();
    assert!(all.contains("five"));
    assert!(all.contains("four"), "the viewport follows the active area again");
}

#[test]
fn mode_get_and_set_round_trip() {
    let mut t = term(10, 3);
    assert!(t.mode(Mode::WRAPAROUND), "DECAWM defaults on");
    t.set_mode(Mode::BRACKETED_PASTE, true).expect("set mode");
    assert!(t.mode(Mode::BRACKETED_PASTE));
    t.set_mode(Mode::BRACKETED_PASTE, false).expect("unset mode");
    assert!(!t.mode(Mode::BRACKETED_PASTE));
}

#[test]
fn reset_clears_the_screen_but_not_the_size() {
    let mut t = term(17, 5);
    t.write(b"\x1b[2J\x1b[Hleftover\x1b]2;t\x07");
    t.reset();
    assert_eq!(t.size(), (17, 5));
    let snap = t.snapshot();
    assert!(snap.rows.iter().all(|r| r.cells.iter().all(|c| c.text.is_empty())));
}

#[test]
fn zero_dimensions_error_rather_than_guess() {
    assert!(Terminal::new(0, 24, 100).is_err());
    assert!(Terminal::new(80, 0, 100).is_err());
}

#[test]
fn terminal_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Terminal>();
}

#[test]
fn terminal_moves_between_threads() {
    let mut t = term(10, 3);
    t.write(b"across\r\n");
    let handle = std::thread::spawn(move || {
        t.write(b"threads\r\n");
        t // move back
    });
    let t = handle.join().expect("thread ran clean");
    assert!(t.text_full().contains("across"));
    assert!(t.text_full().contains("threads"));
}

#[test]
fn hyperlink_uris_dedupe_into_the_side_table() {
    let mut t = term(40, 3);
    t.write(b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07 plain \x1b]8;;https://example.com\x07again\x1b]8;;\x07");
    let snap = t.snapshot();
    assert_eq!(snap.hyperlinks, vec!["https://example.com".to_string()]);
    let linked: Vec<Option<u16>> = snap.rows[0]
        .cells
        .iter()
        .filter(|c| c.hyperlink.is_some())
        .map(|c| c.hyperlink)
        .collect();
    assert_eq!(linked.len(), 9, "'link' + 'again' cells all carry it");
    assert!(linked.iter().all(|i| *i == Some(0)));
}


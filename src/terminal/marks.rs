//! Command marks: where each shell prompt started in the scrollback, so the
//! details panel's Outline can list a pane's commands and scroll back to one.
//!
//! Fed by the reader thread from OSC 133 (`A` prompt start, `C` command start,
//! `D` command done — the same shell-integration marks the daemon sniffs for
//! prompt state). The daemon reports only *whether* the shell is at its prompt;
//! positions have to come from the client, because only the client holds the
//! grid those positions are relative to.
//!
//! # Why a mark stores its text
//!
//! A grid row has no stable identity. Alacritty's `Line` is relative to the
//! viewport, so anything recorded in those coordinates slides as output arrives.
//! Converting to an absolute index from the top of history (`history_size -
//! display_offset + line`) is stable — *until the scrollback fills*. After that
//! alacritty discards the oldest row per new row, every surviving row's absolute
//! index silently decreases, and the amount discarded is not observable from
//! outside the emulator: `history_size` is pinned at the limit, and nothing else
//! exposes the scroll count. (Counting it exactly would mean wrapping
//! `vte::ansi::Handler` to intercept every line-producing sequence — 71 methods,
//! all with no-op defaults, so a future `vte` upgrade that adds one would
//! silently break rendering. Not worth it for this.)
//!
//! So the absolute index is treated as a *hint* and the row's text as the
//! *truth*: each mark records what its row said when it was made, and a reader
//! re-reads the row before trusting the position. A mark whose row no longer
//! matches has drifted out from under us and is reported stale rather than
//! silently scrolling somewhere wrong. Below the scrollback limit — which is
//! where a pane spends most of its life — the hint is exact and the check always
//! passes.

use std::sync::{Arc, Mutex};

/// Cap on retained marks. Deep scrollback holds far more prompts than a panel
/// list is useful at, and the oldest are the likeliest to have drifted anyway.
const MAX_MARKS: usize = 500;

/// One shell prompt, and the command run from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandMark {
    /// Row index from the top of the scrollback at record time — the position
    /// hint. See the module docs for when it stops being exact.
    pub row: i64,
    /// What the row said when the mark was made, used to detect drift. Empty
    /// while the prompt has been printed but nothing has been typed yet.
    pub text: String,
    /// Exit code from `OSC 133;D`, once the command finishes.
    pub exit: Option<i32>,
    /// Whether the command has finished (a `D` mark arrived). Distinct from
    /// `exit.is_some()`: a `D` without a code still means "done".
    pub done: bool,
}

/// A pane's marks, shared between the reader thread (writer) and the UI (reader).
#[derive(Clone, Default)]
pub struct Marks(Arc<Mutex<Vec<CommandMark>>>);

impl Marks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a mark at `row` (`OSC 133;A` — the shell is about to print a
    /// prompt). `text` is the row's current content, which is normally empty at
    /// this point and gets filled in by [`set_text`](Self::set_text) once the
    /// command has been typed.
    pub fn begin(&self, row: i64, text: String) {
        let Ok(mut marks) = self.0.lock() else { return };
        // A prompt redraw (a resize, a `clear`, zle repainting the line) re-emits
        // `A` on the same row. Update in place rather than stacking duplicates.
        if marks.last().is_some_and(|m| m.row == row && !m.done) {
            if let Some(last) = marks.last_mut() {
                last.text = text;
            }
            return;
        }
        marks.push(CommandMark {
            row,
            text,
            exit: None,
            done: false,
        });
        // Trim from the front: oldest marks age out of the scrollback first.
        let overflow = marks.len().saturating_sub(MAX_MARKS);
        if overflow > 0 {
            marks.drain(..overflow);
        }
    }

    /// Attach the command line to the open mark (`OSC 133;C` — the user hit
    /// enter, so the prompt row now holds the command). Ignored when no mark is
    /// open, which is what a `C` without a preceding `A` means.
    pub fn set_text(&self, text: String) {
        let Ok(mut marks) = self.0.lock() else { return };
        if let Some(last) = marks.last_mut() {
            if !last.done {
                last.text = text;
            }
        }
    }

    /// Close the open mark (`OSC 133;D[;exit]`).
    pub fn finish(&self, exit: Option<i32>) {
        let Ok(mut marks) = self.0.lock() else { return };
        if let Some(last) = marks.last_mut() {
            last.done = true;
            last.exit = exit;
        }
    }

    /// Snapshot for rendering, newest last. Marks that never got a command are
    /// dropped: a bare prompt the user typed nothing at is not an outline entry.
    pub fn list(&self) -> Vec<CommandMark> {
        let Ok(marks) = self.0.lock() else {
            return Vec::new();
        };
        marks
            .iter()
            .filter(|m| !m.text.trim().is_empty())
            .cloned()
            .collect()
    }

    /// Drop everything (the pane was cleared, so every position is meaningless).
    pub fn clear(&self) {
        if let Ok(mut marks) = self.0.lock() {
            marks.clear();
        }
    }
}

/// Parse the exit code out of an `OSC 133;D` payload: `D`, `D;0`, `D;1`, and
/// zsh's `D;aborted` all occur. Anything unparseable is "done, code unknown".
pub fn parse_done_exit(payload: &[u8]) -> Option<i32> {
    let rest = payload.strip_prefix(b"D")?;
    let rest = rest.strip_prefix(b";")?;
    std::str::from_utf8(rest).ok()?.trim().parse().ok()
}

/// What a recognized `OSC 133` mark means for the outline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkEvent {
    /// `A` — the shell is about to print a prompt.
    Prompt,
    /// `C;<cmd>` — the command was submitted and its output starts here. tty7's
    /// own shell integration always includes the command line, so the outline
    /// never has to guess it back out of the grid (where it would be tangled up
    /// with the user's prompt string).
    Command(String),
    /// `D[;exit]` — the command finished.
    Done(Option<i32>),
}

/// Finds `OSC 133` marks in the output stream and reports *where* each one lands
/// — the byte offset just past the sequence — so the caller can advance the
/// emulator up to exactly that point and read the grid position there.
///
/// Separate from [`OscTokenizer`](crate::core::osc::OscTokenizer), which reports
/// payloads but not offsets. Carries its state across feeds, so a mark split over
/// two socket reads is still recognized (and attributed to the batch its
/// terminator lands in, which is the correct row either way).
#[derive(Default)]
pub struct MarkScanner {
    state: ScanState,
    /// Payload bytes collected so far, possibly spanning feeds. Bounded: a
    /// "payload" that runs past any plausible command line is a desync, not a
    /// mark, so it's abandoned rather than grown without limit.
    payload: Vec<u8>,
}

#[derive(Default, PartialEq, Eq)]
enum ScanState {
    /// Ordinary output.
    #[default]
    Text,
    /// Saw `ESC`, waiting to see whether `]` follows.
    Esc,
    /// Inside an OSC payload, collecting until BEL or ST.
    Osc,
    /// Saw `ESC` inside an OSC payload — an ST (`ESC \`) if `\` follows.
    OscEsc,
}

/// Ceiling on a collected OSC payload. Long enough for any real command line,
/// short enough that a stream that never terminates its OSC can't grow a buffer
/// unboundedly.
const MAX_PAYLOAD: usize = 64 * 1024;

impl MarkScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one batch. `on_mark(offset, event)` fires for each recognized mark,
    /// where `offset` is an index into `bytes` just past the mark's terminator.
    /// Ordinary output is the overwhelming majority of every batch, and the only
    /// byte that can end it is `ESC` — so that state skips ahead with SIMD
    /// `memchr` rather than stepping per byte, exactly as
    /// [`OscTokenizer::feed`](crate::core::osc::OscTokenizer::feed) does. This
    /// scanner runs over every batch the client receives, alongside three
    /// tokenizers that already did this; measured on an 8 MB batch of plausible
    /// output it was the difference between 1.6 GB/s and 8.3 GB/s.
    pub fn feed(&mut self, bytes: &[u8], mut on_mark: impl FnMut(usize, MarkEvent)) {
        let mut i = 0;
        while i < bytes.len() {
            if self.state == ScanState::Text {
                // No ESC in the rest of the batch means nothing here can matter:
                // the state stays `Text`, which is where the next feed resumes.
                let Some(off) = memchr::memchr(0x1b, &bytes[i..]) else {
                    return;
                };
                self.state = ScanState::Esc;
                i += off + 1;
                continue;
            }
            let b = bytes[i];
            match self.state {
                // Handled by the skip-ahead above.
                ScanState::Text => unreachable!(),
                ScanState::Esc => {
                    if b == b']' {
                        self.state = ScanState::Osc;
                        self.payload.clear();
                    } else {
                        // Some other escape sequence; `ESC ESC` restarts.
                        self.state = if b == 0x1b {
                            ScanState::Esc
                        } else {
                            ScanState::Text
                        };
                    }
                }
                ScanState::Osc => match b {
                    0x07 => {
                        if let Some(ev) = self.take() {
                            on_mark(i + 1, ev);
                        }
                        self.state = ScanState::Text;
                    }
                    0x1b => self.state = ScanState::OscEsc,
                    _ => {
                        if self.payload.len() < MAX_PAYLOAD {
                            self.payload.push(b);
                        } else {
                            // Runaway payload: give up on this sequence rather
                            // than buffer the rest of the stream into it.
                            self.state = ScanState::Text;
                            self.payload.clear();
                        }
                    }
                },
                ScanState::OscEsc => {
                    if b == b'\\' {
                        if let Some(ev) = self.take() {
                            on_mark(i + 1, ev);
                        }
                        self.state = ScanState::Text;
                    } else {
                        // Not an ST after all — the ESC was payload. Bounded like
                        // the ordinary payload byte below it: a stream of bare
                        // ESCs inside an unterminated OSC would otherwise grow the
                        // buffer a byte at a time, never reaching the check there.
                        if self.payload.len() < MAX_PAYLOAD {
                            self.payload.push(0x1b);
                            self.state = ScanState::Osc;
                        } else {
                            self.state = ScanState::Text;
                            self.payload.clear();
                        }
                    }
                }
            }
            i += 1;
        }
    }

    /// Interpret the collected payload, clearing it either way.
    fn take(&mut self) -> Option<MarkEvent> {
        let payload = std::mem::take(&mut self.payload);
        let body = payload.strip_prefix(b"133;")?;
        match body.first()? {
            b'A' => Some(MarkEvent::Prompt),
            b'C' => {
                // `C` alone (no command) still marks output start; the shells
                // that can't report the line send it bare.
                let cmd = body
                    .strip_prefix(b"C;")
                    .map(|c| String::from_utf8_lossy(c).into_owned())
                    .unwrap_or_default();
                Some(MarkEvent::Command(cmd))
            }
            b'D' => Some(MarkEvent::Done(parse_done_exit(body))),
            // `B` (prompt end) and `V` (tty7's edit-mode extension) carry no
            // position the outline cares about.
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_with_no_command_is_not_an_entry() {
        let marks = Marks::new();
        marks.begin(10, String::new());
        assert!(
            marks.list().is_empty(),
            "an empty prompt the user walked away from isn't a command"
        );
        marks.set_text("cargo build".into());
        assert_eq!(marks.list().len(), 1);
    }

    #[test]
    fn a_prompt_redraw_updates_in_place() {
        let marks = Marks::new();
        marks.begin(10, String::new());
        marks.set_text("cargo t".into());
        // zle repaints the prompt on the same row (a resize, a completion menu
        // closing) and the shell re-emits `A`.
        marks.begin(10, "cargo test".into());
        let got = marks.list();
        assert_eq!(got.len(), 1, "a redraw is the same prompt, not a new one");
        assert_eq!(got[0].text, "cargo test");
    }

    #[test]
    fn a_new_prompt_after_a_finished_command_is_a_new_entry() {
        let marks = Marks::new();
        marks.begin(10, String::new());
        marks.set_text("ls".into());
        marks.finish(Some(0));
        // Same row is possible after a `clear`.
        marks.begin(10, String::new());
        marks.set_text("pwd".into());
        let got = marks.list();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].exit, Some(0));
        assert!(!got[1].done);
    }

    #[test]
    fn marks_are_capped_from_the_front() {
        let marks = Marks::new();
        for i in 0..(MAX_MARKS + 10) {
            marks.begin(i as i64, format!("cmd{i}"));
            marks.finish(Some(0));
        }
        let got = marks.list();
        assert_eq!(got.len(), MAX_MARKS);
        assert_eq!(got[0].text, "cmd10", "the oldest aged out, not the newest");
    }

    /// Collect `(offset, event)` pairs from feeding `chunks` in order, so a test
    /// can assert on a stream split at arbitrary boundaries.
    fn scan(chunks: &[&[u8]]) -> Vec<(usize, MarkEvent)> {
        let mut scanner = MarkScanner::new();
        let mut out = Vec::new();
        for chunk in chunks {
            scanner.feed(chunk, |off, ev| out.push((off, ev)));
        }
        out
    }

    /// The `Text` state skips to the next `ESC` with `memchr` instead of
    /// walking byte by byte, which means it — not the loop — decides where
    /// scanning resumes. Splitting one stream at *every* offset and comparing
    /// against the unsplit scan pins that: an off-by-one in the resume index,
    /// or a state that the skip forgets to carry across a feed, shows up as a
    /// shifted offset or a lost mark at exactly one split point.
    #[test]
    fn splitting_anywhere_yields_the_same_marks() {
        // Deliberately awkward: bare ESCs, an `ESC ESC` restart, a non-OSC
        // escape, an ST-terminated mark and a BEL-terminated one.
        let stream: &[u8] =
            b"out\x1b\x1b[32mmore\x1b]133;C;git status\x07text\x1b]133;D;0\x1b\\tail\x1b";
        let whole = scan(&[stream]);
        assert_eq!(whole.len(), 2, "both marks found in one pass");

        for at in 0..=stream.len() {
            // Offsets are relative to the feed they came from, so rebase the
            // second half onto the whole stream before comparing.
            let mut scanner = MarkScanner::new();
            let mut got = Vec::new();
            scanner.feed(&stream[..at], |off, ev| got.push((off, ev)));
            scanner.feed(&stream[at..], |off, ev| got.push((at + off, ev)));
            assert_eq!(got, whole, "splitting at {at} changed the marks");
        }
    }

    #[test]
    fn reports_marks_just_past_their_terminator() {
        let got = scan(&[b"ab\x1b]133;A\x07cd"]);
        assert_eq!(got, vec![(10, MarkEvent::Prompt)]);
        // The offset must point past the BEL, so advancing `bytes[..offset]`
        // consumes the whole sequence and nothing of what follows.
        assert_eq!(&b"ab\x1b]133;A\x07cd"[10..], b"cd");
    }

    #[test]
    fn carries_a_mark_split_across_two_feeds() {
        let got = scan(&[b"out\x1b]13", b"3;C;cargo build\x07more"]);
        assert_eq!(
            got,
            vec![(16, MarkEvent::Command("cargo build".into()))],
            "the mark is attributed to the batch its terminator lands in"
        );
    }

    #[test]
    fn accepts_st_terminated_marks() {
        // `ESC \` instead of BEL — both are legal OSC terminators and the
        // integrations use ST on some shells.
        let got = scan(&[b"\x1b]133;D;130\x1b\\"]);
        assert_eq!(got, vec![(13, MarkEvent::Done(Some(130)))]);
    }

    #[test]
    fn ignores_other_osc_sequences() {
        let got = scan(&[b"\x1b]0;a title\x07\x1b]7;file://h/x\x07\x1b]133;B\x07"]);
        assert!(
            got.is_empty(),
            "titles, cwd reports and prompt-end carry nothing the outline wants"
        );
    }

    #[test]
    fn a_command_containing_semicolons_survives_intact() {
        let got = scan(&[b"\x1b]133;C;for i in a b; do echo $i; done\x07"]);
        assert_eq!(
            got,
            vec![(
                39,
                MarkEvent::Command("for i in a b; do echo $i; done".into())
            )],
            "only the first two fields are structure; the rest is the command"
        );
    }

    #[test]
    fn an_unterminated_payload_cannot_grow_without_bound() {
        let mut scanner = MarkScanner::new();
        let mut fired = 0;
        scanner.feed(b"\x1b]133;C;", |_, _| fired += 1);
        for _ in 0..40 {
            scanner.feed(&vec![b'x'; 4096], |_, _| fired += 1);
        }
        assert_eq!(fired, 0, "never terminated, so never reported");
        assert!(scanner.payload.len() <= MAX_PAYLOAD);
    }

    #[test]
    fn parses_done_payloads() {
        assert_eq!(parse_done_exit(b"D;0"), Some(0));
        assert_eq!(parse_done_exit(b"D;130"), Some(130));
        assert_eq!(parse_done_exit(b"D"), None, "done, code unknown");
        assert_eq!(parse_done_exit(b"D;aborted"), None);
        assert_eq!(parse_done_exit(b"C"), None, "not a done mark at all");
    }
}

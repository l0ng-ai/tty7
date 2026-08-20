//! Where each turn of a coding agent's conversation sits in the scrollback.
//!
//! The hooks tty7 installs into Claude Code and friends already announce a
//! turn's start and end over the pty, as an OSC 777 the daemon reads for the
//! pane's status dot. Those same bytes arrive at the client, and *here* they
//! are worth something else: the byte offset a `prompt-submit` lands on is a
//! position in the stream, so advancing the emulator to exactly there and
//! reading the cursor gives the row that turn began on. That is an outline of
//! the conversation, and a place to scroll back to.
//!
//! # Why the anchor is not the whole answer
//!
//! The hook is a subprocess writing to the controlling tty while the agent's
//! own renderer writes to it too. Claude Code repaints in place with ink, so
//! the cursor at the moment the hook's bytes land sits wherever the last
//! repaint left it — inside the live region at the bottom, a few rows off from
//! where the prompt's echo finally comes to rest. The anchor is close, not
//! exact.
//!
//! So the anchor is a *hint*, and [`AgentTurn::text`] is the correction: the
//! first line of what the user typed, which the view looks for around the
//! anchor when the jump happens (by then it has long been drawn). The anchor
//! narrows the search and orders the list; the text lands the jump.
//!
//! # Why rows and not the transcript file
//!
//! Claude Code keeps a JSONL transcript, and reading it would give the
//! assistant's side of the conversation too. It would also only work for
//! Claude, only for a pane whose agent runs on this machine, and only for a
//! path this process is allowed to read. An OSC comes back through the pty
//! from wherever the agent actually runs — over ssh, inside a container, in a
//! remote workspace — with no file access and no per-agent format. What is
//! lost is the assistant's text; what is kept is every host tty7 supports.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::term::{Term, TermMode};

use crate::core::cli_agent::{AgentEventKind, parse_agent_event};
use crate::core::osc::OscTokenizer;

/// Turns kept per pane. A long agent session is tens of turns, not hundreds;
/// this is the bound that keeps a runaway hook from growing the list without
/// end, not a number anyone should reach.
const MAX_TURNS: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTurn {
    /// Absolute scrollback row: `history_size + grid line`, counting from the
    /// oldest line the emulator still holds. `None` when the turn began on the
    /// alt screen, which has no scrollback to point into.
    ///
    /// The count it is relative to shrinks once the scrollback limit starts
    /// discarding lines, which slides every anchor by the same amount. That is
    /// what `text` is for.
    pub row: Option<i64>,
    /// The first line of the prompt, as the hook clamped it. Empty when the
    /// agent's hook does not report prompts.
    pub text: String,
    /// The turn ended (a `stop` event arrived).
    pub done: bool,
    /// Identity for the UI, so a list that grows underneath a click still
    /// points at the same turn.
    pub id: u64,
}

#[derive(Clone, Default)]
pub struct AgentTurns(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    turns: Vec<AgentTurn>,
    next_id: u64,
}

impl AgentTurns {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(&self) -> Vec<AgentTurn> {
        let Ok(inner) = self.0.lock() else {
            return Vec::new();
        };
        inner.turns.clone()
    }

    /// Drop everything, for the same reason the image store is dropped: the
    /// rows these point into are gone (`clear_scrollback`, a relink that resets
    /// the grid) or belong to a different conversation (a fresh session).
    pub fn clear(&self) {
        if let Ok(mut inner) = self.0.lock() {
            inner.turns.clear();
        }
    }

    /// Move a turn onto the row the view found its text on. Jumping twice
    /// should not search twice, and should not land in two different places.
    pub fn recenter(&self, id: u64, row: i64) {
        let Ok(mut inner) = self.0.lock() else { return };
        if let Some(turn) = inner.turns.iter_mut().find(|t| t.id == id) {
            turn.row = Some(row);
        }
    }

    /// Read the row the emulator is on and record the turn there. Called with
    /// the emulator advanced to exactly the byte after the event's terminator.
    pub fn apply<T: EventListener>(&self, term: &Term<T>, cut: TurnCut) {
        match cut {
            TurnCut::Begin { text } => {
                // The alt screen is a scratch surface with no history behind
                // it, so there is no row to come back to. The turn still
                // belongs in the list — an agent that renders there (Codex
                // does) has a conversation like any other — it just cannot be
                // jumped to.
                let row = (!term.mode().contains(TermMode::ALT_SCREEN)).then(|| {
                    let grid = term.grid();
                    grid.history_size() as i64 + i64::from(grid.cursor.point.line.0)
                });
                self.begin(row, text.unwrap_or_default());
            }
            TurnCut::End => self.finish(),
            TurnCut::Reset => self.clear(),
        }
    }

    fn begin(&self, row: Option<i64>, text: String) {
        let Ok(mut inner) = self.0.lock() else { return };
        // One turn, announced twice. Hooks are not guaranteed to fire once —
        // an agent reading two settings sources runs the same command twice,
        // and a resumed session re-announces the turn it is resuming. What
        // makes it the same turn is that the one before it never ended: a real
        // repeat of a prompt can only come after the answer to the first, and
        // an answer always brings a `stop`.
        if let Some(last) = inner.turns.last_mut()
            && !last.done
            && last.text == text
        {
            // The earlier anchor is the better one — it was taken before the
            // agent had drawn anything in reply — but take a row over nothing.
            if last.row.is_none() {
                last.row = row;
            }
            return;
        }
        let id = inner.next_id;
        inner.next_id += 1;
        inner.turns.push(AgentTurn {
            row,
            text,
            done: false,
            id,
        });
        let overflow = inner.turns.len().saturating_sub(MAX_TURNS);
        if overflow > 0 {
            inner.turns.drain(..overflow);
        }
    }

    fn finish(&self) {
        let Ok(mut inner) = self.0.lock() else { return };
        if let Some(last) = inner.turns.last_mut() {
            last.done = true;
        }
    }
}

/// Longest needle taken from a prompt. The point of a cap is that the text has
/// to sit on *one* grid row to be findable, and a prompt longer than the pane
/// is wide wrapped when it was drawn.
const NEEDLE_MAX: usize = 32;

/// Shortest needle worth searching for *by containment*. Below this, `y` or
/// `go` occurs inside half the rows on screen and the closest match to the
/// anchor would be noise dressed up as precision. The exact match below has no
/// such floor: a row that *is* the prompt is the prompt however short it is.
const NEEDLE_MIN: usize = 3;

/// What a TUI draws in front of what the user typed. Stripping one of these
/// turns the row Claude Code renders — `> restore the outline` — back into the
/// prompt the hook reported, so the two can be compared as equals.
const PROMPT_MARKERS: &[char] = &[
    '>', '❯', '›', '»', '〉', '⟩', '▶', '●', '•', '│', '|', '$', '#', '*',
];

/// Rows to look at either side of the anchor before widening to the whole
/// scrollback. Two screens covers the live region an agent repaints in, which
/// is the distance the anchor can be off by.
const NEAR: i64 = 120;

/// The row a turn's prompt was actually drawn on, or `anchor` when there is
/// nothing to go on.
///
/// The anchor says where the cursor was when the hook fired, which is a few
/// rows from where the prompt's echo settled (see the module docs), and drifts
/// further once the scrollback limit starts discarding lines out from under
/// every anchor at once. The prompt's own text does not drift, so it is the
/// better answer whenever it can be found — near the anchor first, since a
/// prompt asked twice should resolve to the turn that was clicked.
pub fn locate<T: EventListener>(term: &Term<T>, anchor: i64, text: &str) -> i64 {
    let Some(needle) = needle(text) else {
        return anchor;
    };
    let grid = term.grid();
    let last = grid.history_size() as i64 + grid.screen_lines() as i64 - 1;
    let near = NEAR.max(2 * grid.screen_lines() as i64);
    let (lo, hi) = ((anchor - near).max(0), (anchor + near).min(last));
    // Exact first, everywhere, before settling for containment: the row that
    // *is* the prompt beats a row that merely mentions it, even when the
    // mention is closer. `hi` appears inside a dozen rows of any answer; only
    // one row is `> hi`.
    Match::Exact
        .nearest(term, anchor, needle, lo, hi)
        .or_else(|| Match::Exact.nearest(term, anchor, needle, 0, last))
        .or_else(|| Match::Loose.nearest(term, anchor, needle, lo, hi))
        .or_else(|| Match::Loose.nearest(term, anchor, needle, 0, last))
        .unwrap_or(anchor)
}

/// How hard a row has to try to count as the one the prompt was drawn on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Match {
    /// The row, minus the marker the TUI drew in front of it, starts with the
    /// prompt. This is what an agent's echo of the user's line looks like.
    Exact,
    /// The row mentions the prompt somewhere. The fallback for a TUI that
    /// frames its messages some other way — and the reason for [`NEEDLE_MIN`].
    Loose,
}

fn needle(text: &str) -> Option<&str> {
    let text = text.trim();
    let end = text
        .char_indices()
        .nth(NEEDLE_MAX)
        .map_or(text.len(), |(i, _)| i);
    Some(&text[..end]).filter(|n| !n.is_empty())
}

impl Match {
    /// The row closest to `anchor` in `lo..=hi` that matches, found by walking
    /// outwards so the first hit is the answer.
    fn nearest<T: EventListener>(
        self,
        term: &Term<T>,
        anchor: i64,
        needle: &str,
        lo: i64,
        hi: i64,
    ) -> Option<i64> {
        if lo > hi || (self == Match::Loose && needle.chars().count() < NEEDLE_MIN) {
            return None;
        }
        let reach = (anchor - lo).max(hi - anchor).max(0);
        for step in 0..=reach {
            let probes = [anchor - step, anchor + step];
            for &row in &probes[..if step == 0 { 1 } else { 2 }] {
                if (lo..=hi).contains(&row) && self.holds(term, row, needle) {
                    return Some(row);
                }
            }
        }
        None
    }

    fn holds<T: EventListener>(self, term: &Term<T>, row: i64, needle: &str) -> bool {
        let Some(text) = row_text(term, row) else {
            return false;
        };
        match self {
            Match::Loose => text.contains(needle),
            Match::Exact => {
                let line = text.trim();
                let line = match line.chars().next() {
                    Some(c) if PROMPT_MARKERS.contains(&c) => &line[c.len_utf8()..],
                    _ => line,
                };
                line.trim_start().starts_with(needle)
            }
        }
    }
}

/// One scrollback row as text, with the spacer cells that follow double-width
/// glyphs left out — keeping them would break every CJK needle in half.
fn row_text<T: EventListener>(term: &Term<T>, row: i64) -> Option<String> {
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Flags;

    let grid = term.grid();
    let line = row - grid.history_size() as i64;
    if line < -(grid.history_size() as i64) || line >= grid.screen_lines() as i64 {
        return None;
    }
    let line = i32::try_from(line).ok()?;
    let row = &grid[Line(line)];
    let mut text = String::with_capacity(grid.columns());
    for col in 0..grid.columns() {
        let cell = &row[Column(col)];
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        text.push(cell.c);
        if let Some(zerowidth) = cell.zerowidth() {
            text.extend(zerowidth);
        }
    }
    Some(text)
}

/// What an agent event means for the outline, reported at the offset one past
/// the sequence that carried it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnCut {
    Begin {
        text: Option<String>,
    },
    End,
    /// A session started: whatever is in the list belongs to a conversation
    /// that is over. Claude Code sends this on `/clear` and on a resume, both
    /// of which repaint the pane from scratch.
    Reset,
}

/// Byte scanner over the pty stream, reporting the agent events an outline
/// cares about, in ascending offset order — the same shape (and the same
/// contract) as [`ParkedCursorScanner`](crate::terminal::parked_cursor::ParkedCursorScanner).
pub struct AgentTurnScanner {
    tok: OscTokenizer,
}

impl Default for AgentTurnScanner {
    fn default() -> Self {
        Self {
            tok: OscTokenizer::new(&[b"777"]),
        }
    }
}

impl AgentTurnScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets a sequence in progress. A replayed snapshot is a new stream, and
    /// half an OSC from the old one must not join up with it.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn feed(&mut self, bytes: &[u8], mut on_cut: impl FnMut(usize, TurnCut)) {
        self.tok.feed_at(bytes, |at, payload| {
            let Some(ev) = parse_agent_event(payload) else {
                return;
            };
            match ev.kind {
                AgentEventKind::PromptSubmit => on_cut(at, TurnCut::Begin { text: ev.prompt }),
                AgentEventKind::Stop => on_cut(at, TurnCut::End),
                AgentEventKind::SessionStart => on_cut(at, TurnCut::Reset),
                // The rest move the status dot, not the conversation: a
                // permission prompt or a tool completion happens *inside* a
                // turn that is already in the list.
                _ => {}
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::vte::ansi::Processor;

    fn event(kind: &str, prompt: Option<&str>) -> Vec<u8> {
        let mut body = serde_json::json!({
            "v": 1,
            "agent": "claude",
            "event": kind,
        });
        if let Some(p) = prompt {
            body["prompt"] = serde_json::Value::String(p.to_string());
        }
        format!(
            "\x1b]777;notify;{};{body}\x07",
            crate::core::cli_agent::AGENT_EVENT_SENTINEL
        )
        .into_bytes()
    }

    fn cuts(chunks: &[&[u8]]) -> Vec<(usize, TurnCut)> {
        let mut scanner = AgentTurnScanner::new();
        let mut got = Vec::new();
        for chunk in chunks {
            scanner.feed(chunk, |at, cut| got.push((at, cut)));
        }
        got
    }

    /// Drives a stream through the emulator the way the reader does — advance
    /// to each cut, act on it, carry on — and reports the turns it recorded.
    fn turns_after(stream: &[u8]) -> Vec<AgentTurn> {
        let mut term = Term::new(
            alacritty_terminal::term::Config {
                scrolling_history: 1000,
                ..Default::default()
            },
            &crate::terminal::size::TermSize::new(80, 24),
            VoidListener,
        );
        let mut parser: Processor = Processor::new();
        let mut scanner = AgentTurnScanner::new();
        let turns = AgentTurns::new();

        let mut cuts = Vec::new();
        scanner.feed(stream, |off, cut| cuts.push((off, cut)));
        let mut at = 0;
        for (off, cut) in cuts {
            parser.advance(&mut term, &stream[at..off]);
            at = off;
            turns.apply(&term, cut);
        }
        parser.advance(&mut term, &stream[at..]);
        turns.list()
    }

    #[test]
    fn a_prompt_and_its_stop_are_one_turn() {
        let mut stream = event("prompt-submit", Some("restore the outline"));
        stream.extend_from_slice(&event("stop", None));
        let got = turns_after(&stream);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "restore the outline");
        assert!(got[0].done);
    }

    #[test]
    fn a_turn_anchors_to_the_row_the_prompt_arrived_on() {
        let mut stream = b"one\r\ntwo\r\nthree\r\n".to_vec();
        stream.extend_from_slice(&event("prompt-submit", Some("go")));
        assert_eq!(
            turns_after(&stream)[0].row,
            Some(3),
            "three lines written, so the cursor is on the fourth row"
        );
    }

    #[test]
    fn an_anchor_counts_from_the_oldest_line_still_held() {
        // Fill past a screen so lines are in history, and the anchor has to
        // count them rather than the visible rows.
        let mut stream = "x\r\n".repeat(30).into_bytes();
        stream.extend_from_slice(&event("prompt-submit", Some("go")));
        assert_eq!(
            turns_after(&stream)[0].row,
            Some(30),
            "30 rows written: 7 scrolled into history, cursor on screen line 23"
        );
    }

    #[test]
    fn a_turn_on_the_alt_screen_has_no_row_to_return_to() {
        let mut stream = b"\x1b[?1049h".to_vec();
        stream.extend_from_slice(&event("prompt-submit", Some("go")));
        let got = turns_after(&stream);
        assert_eq!(got.len(), 1, "still a turn, still listed");
        assert_eq!(got[0].row, None, "nothing behind the alt screen to jump to");
    }

    #[test]
    fn a_session_start_drops_the_previous_conversation() {
        let mut stream = event("prompt-submit", Some("first"));
        stream.extend_from_slice(&event("stop", None));
        stream.extend_from_slice(&event("session-start", None));
        stream.extend_from_slice(&event("prompt-submit", Some("second")));
        let got = turns_after(&stream);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "second");
    }

    #[test]
    fn events_that_only_move_the_status_dot_are_not_turns() {
        for kind in ["notification", "permission-request", "tool-complete"] {
            assert!(
                cuts(&[&event(kind, None)]).is_empty(),
                "{kind} happens inside a turn, it does not start one"
            );
        }
    }

    #[test]
    fn other_osc_777_notifications_are_left_alone() {
        assert!(
            cuts(&[b"\x1b]777;notify;Build;finished\x07"]).is_empty(),
            "a plain desktop notification is not an agent event"
        );
    }

    #[test]
    fn a_cut_lands_one_past_its_sequence() {
        let ev = event("stop", None);
        let mut stream = b"before".to_vec();
        stream.extend_from_slice(&ev);
        stream.extend_from_slice(b"after");
        let got = cuts(&[&stream]);
        assert_eq!(got.len(), 1);
        assert_eq!(
            &stream[got[0].0..],
            b"after",
            "the emulator must be advanced to just past the event"
        );
    }

    #[test]
    fn an_event_split_across_reads_still_arrives() {
        let ev = event("prompt-submit", Some("split me"));
        let (head, tail) = ev.split_at(ev.len() / 2);
        assert_eq!(
            cuts(&[head, tail]),
            vec![(
                tail.len(),
                TurnCut::Begin {
                    text: Some("split me".into())
                }
            )],
            "the cut is attributed to the batch its terminator lands in"
        );
    }

    /// A pane holding `lines`, one per row, with the first of them at row 0.
    fn painted(lines: &[&str]) -> Term<VoidListener> {
        let mut term = Term::new(
            alacritty_terminal::term::Config {
                scrolling_history: 1000,
                ..Default::default()
            },
            &crate::terminal::size::TermSize::new(80, 24),
            VoidListener,
        );
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, lines.join("\r\n").as_bytes());
        term
    }

    #[test]
    fn a_prompt_is_found_a_few_rows_off_its_anchor() {
        let mut lines = vec!["boot"; 40];
        lines[30] = "> restore the outline please";
        let term = painted(&lines);
        assert_eq!(
            locate(&term, 33, "restore the outline please"),
            30,
            "the anchor lands in the live region; the text says where the turn is"
        );
    }

    #[test]
    fn the_same_prompt_twice_resolves_to_the_one_that_was_clicked() {
        let mut lines = vec!["boot"; 200];
        lines[20] = "> run the tests";
        lines[150] = "> run the tests";
        let term = painted(&lines);
        assert_eq!(locate(&term, 22, "run the tests"), 20);
        assert_eq!(locate(&term, 148, "run the tests"), 150);
    }

    #[test]
    fn a_prompt_far_from_its_anchor_is_still_found() {
        // What a scrollback that has started discarding lines looks like: every
        // anchor slid, and the near window no longer covers the distance.
        let mut lines = vec!["boot"; 600];
        lines[80] = "> the anchor drifted away from me";
        let term = painted(&lines);
        assert_eq!(locate(&term, 500, "the anchor drifted away from me"), 80);
    }

    #[test]
    fn a_wide_glyph_prompt_survives_the_spacer_cells() {
        let mut lines = vec!["boot"; 40];
        lines[12] = "> 把大纲恢复一下";
        let term = painted(&lines);
        assert_eq!(
            locate(&term, 15, "把大纲恢复一下"),
            12,
            "the cell after a double-width glyph is a spacer, not part of the text"
        );
    }

    #[test]
    fn a_two_letter_prompt_still_lands_on_its_own_row() {
        // The row the agent echoed the prompt on is `> hi`; every row of the
        // answer under it may well contain "hi" too. Exactness, not length, is
        // what makes the short one safe.
        let mut lines = vec!["boot"; 40];
        lines[12] = "> hi";
        lines[14] = "Hi! What can I help you with?";
        lines[16] = "I see you are on this branch";
        let term = painted(&lines);
        assert_eq!(locate(&term, 15, "hi"), 12);
    }

    #[test]
    fn the_marker_a_tui_draws_in_front_does_not_hide_the_prompt() {
        for marker in ["> ", "❯ ", "〉", "│ ", "▶ "] {
            let mut lines = vec!["boot".to_string(); 40];
            lines[9] = format!("{marker}run the tests");
            let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
            let term = painted(&borrowed);
            assert_eq!(locate(&term, 13, "run the tests"), 9, "marker {marker:?}");
        }
    }

    #[test]
    fn a_row_that_is_the_prompt_beats_a_nearer_row_that_merely_mentions_it() {
        let mut lines = vec!["boot"; 60];
        lines[20] = "> run the tests";
        lines[30] = "sure, I will run the tests now";
        let term = painted(&lines);
        assert_eq!(
            locate(&term, 31, "run the tests"),
            20,
            "the echo is the turn; the sentence about it is the answer"
        );
    }

    #[test]
    fn text_too_short_to_be_distinctive_leaves_the_anchor_alone() {
        let mut lines = vec!["y"; 40];
        lines[5] = "y";
        let term = painted(&lines);
        assert_eq!(
            locate(&term, 30, "y"),
            30,
            "a needle that matches everywhere is worse than the anchor"
        );
    }

    #[test]
    fn a_prompt_that_was_never_drawn_leaves_the_anchor_alone() {
        let term = painted(&vec!["boot"; 40]);
        assert_eq!(locate(&term, 12, "nothing on screen says this"), 12);
    }

    #[test]
    fn a_turn_announced_twice_is_still_one_turn() {
        // What a hook that fires from two settings sources produces: the same
        // prompt twice, then the same stop twice.
        let mut stream = event("prompt-submit", Some("hi"));
        stream.extend_from_slice(&event("prompt-submit", Some("hi")));
        stream.extend_from_slice(&event("stop", None));
        stream.extend_from_slice(&event("stop", None));
        let got = turns_after(&stream);
        assert_eq!(got.len(), 1, "one prompt, however many times announced");
        assert!(got[0].done);
    }

    #[test]
    fn the_first_anchor_of_a_repeated_announcement_is_the_one_kept() {
        let mut stream = b"a\r\nb\r\n".to_vec();
        stream.extend_from_slice(&event("prompt-submit", Some("hi")));
        stream.extend_from_slice(b"c\r\nd\r\n");
        stream.extend_from_slice(&event("prompt-submit", Some("hi")));
        assert_eq!(
            turns_after(&stream)[0].row,
            Some(2),
            "the earlier anchor was taken before the agent drew its reply"
        );
    }

    #[test]
    fn the_same_prompt_asked_again_after_an_answer_is_a_new_turn() {
        let mut stream = event("prompt-submit", Some("hi"));
        stream.extend_from_slice(&event("stop", None));
        stream.extend_from_slice(&event("prompt-submit", Some("hi")));
        let got = turns_after(&stream);
        assert_eq!(got.len(), 2, "an answer came between them, so they differ");
        assert!(got[0].done && !got[1].done);
    }

    #[test]
    fn turns_are_capped_from_the_front() {
        let turns = AgentTurns::new();
        for i in 0..(MAX_TURNS + 10) {
            turns.begin(Some(i as i64), format!("turn {i}"));
        }
        let got = turns.list();
        assert_eq!(got.len(), MAX_TURNS);
        assert_eq!(
            got[0].text, "turn 10",
            "the oldest aged out, not the newest"
        );
    }

    #[test]
    fn recentring_moves_the_anchor_by_id_not_by_position() {
        let turns = AgentTurns::new();
        turns.begin(Some(10), "first".into());
        turns.begin(Some(20), "second".into());
        let id = turns.list()[0].id;
        turns.recenter(id, 12);
        assert_eq!(turns.list()[0].row, Some(12));
        assert_eq!(turns.list()[1].row, Some(20), "its neighbour is untouched");
    }
}

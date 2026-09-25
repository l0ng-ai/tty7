//! Finding one command's output and exit code in a pane's byte stream.
//!
//! `tty7 exec` types a line at a shell's prompt and has to know two things the
//! pty alone never says: where the command's output starts and ends, and how it
//! exited. The shell integration says both, in OSC 133 marks: `C` right before
//! the command runs, `D;<code>` the moment it is over, `A` when the next prompt
//! opens. The daemon already reads those marks — it is what sends `Prompt` —
//! but only as a state, not as a position in the bytes, so the positions are
//! found here, in the same stream the daemon read them from.
//!
//! Which of the two gets the last word is deliberate. The byte marks say
//! *where*; the daemon's `Prompt` report says *when*, because it is the one
//! that has already set aside marks relayed by a program running in the
//! foreground (a nested shell, an `ssh`) — a `D` in the bytes is only the end
//! once the daemon agrees the shell is back at its prompt.

use tty7_core::core::osc::OscTokenizer;
use tty7_core::daemon::protocol::WinSize;

/// Output kept past this is dropped from the front: the end of a long build is
/// the part worth reading, and a command that prints without bound must not
/// take the CLI's memory with it. The pane's own scrollback is the same size.
pub const KEEP_OUTPUT: usize = 8 * 1024 * 1024;

const MARK_D: &[u8] = b"\x1b]133;D";
const MARK_A: &[u8] = b"\x1b]133;A";

/// How an `exec` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecEnd {
    /// The pane's shell has never sent a prompt mark, so nothing would ever
    /// say the command is over. Nothing was sent.
    NoIntegration,
    /// The pane is not at a prompt — something already runs there. Nothing
    /// was sent.
    Busy,
    /// The shell is back at its prompt. `ran` is false when it came back
    /// without running anything — a syntax error, most likely — in which case
    /// there is no exit code to report and the output is what the shell said
    /// about the line.
    Finished { exit: Option<i32>, ran: bool },
    /// The deadline passed first. `started` says whether the shell ever
    /// began the command: when it did not, the line is most likely sitting on
    /// a continuation prompt (an unclosed quote) rather than running.
    TimedOut { started: bool },
    /// The pane went away before its shell came back to a prompt.
    PaneExited(Option<i32>),
}

/// What an `exec` saw: how it ended, and the output that belongs to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRun {
    pub end: ExecEnd,
    pub output: Vec<u8>,
    /// Bytes of output dropped from the front to stay under [`KEEP_OUTPUT`].
    pub dropped: usize,
    /// The pane's size, which is what `--plain` replays the output at.
    pub size: WinSize,
}

/// The pane's output from the moment the line was sent, and where the marks
/// that bound the command fell in it.
pub struct Transcript {
    tok: OscTokenizer,
    bytes: Vec<u8>,
    /// Absolute offset of `bytes[0]` — everything before it was dropped.
    base: usize,
    /// One past the command's `C` mark: where its output starts.
    started: Option<usize>,
    /// Where the newest `D` after `started` begins: where the output ends.
    ended: Option<usize>,
    /// Where an `A` that arrived with no `C` before it begins — the shell drew
    /// a fresh prompt without running the line.
    reprompted: Option<usize>,
    exit_mark: Option<i32>,
}

/// What the daemon's `Prompt` report made of the pane.
pub struct Finished {
    pub exit: Option<i32>,
    pub ran: bool,
}

impl Default for Transcript {
    fn default() -> Transcript {
        Transcript::new()
    }
}

impl Transcript {
    pub fn new() -> Transcript {
        Transcript {
            tok: OscTokenizer::new(&[b"133"]),
            bytes: Vec::new(),
            base: 0,
            started: None,
            ended: None,
            reprompted: None,
            exit_mark: None,
        }
    }

    /// Take the next chunk of the pane's output.
    pub fn output(&mut self, chunk: &[u8]) {
        let at = self.base + self.bytes.len();
        self.bytes.extend_from_slice(chunk);
        let (bytes, base) = (&self.bytes, self.base);
        let (mut started, mut ended, mut reprompted, mut exit_mark) =
            (self.started, self.ended, self.reprompted, self.exit_mark);
        self.tok.feed_at(chunk, |end, payload| {
            let end = at + end;
            // Where the sequence that just ended began: the newest copy of its
            // introducer before its terminator. Payloads carry no ESC, so the
            // search cannot land inside a different sequence.
            let begin = |intro: &[u8]| {
                let upto = end - base;
                bytes[..upto]
                    .windows(intro.len())
                    .rposition(|w| w == intro)
                    .map_or(end, |at| base + at)
            };
            match payload.get(4) {
                // Only the first: a command that prints a `C` of its own must
                // not move the start of its own output.
                Some(b'C') if started.is_none() => started = Some(end),
                Some(b'D') if started.is_some() => {
                    ended = Some(begin(MARK_D));
                    exit_mark = payload
                        .strip_prefix(b"133;D;")
                        .and_then(|c| std::str::from_utf8(c).ok())
                        .and_then(|c| c.trim().parse().ok());
                }
                Some(b'A') if started.is_none() && reprompted.is_none() => {
                    reprompted = Some(begin(MARK_A));
                }
                _ => {}
            }
        });
        (self.started, self.ended, self.reprompted, self.exit_mark) =
            (started, ended, reprompted, exit_mark);
    }

    /// The daemon's report that the pane's shell is (or is not) at a prompt.
    /// `Some` once the command is over.
    pub fn prompt(&self, at_prompt: bool, last_exit: Option<i32>) -> Option<Finished> {
        if !at_prompt {
            return None;
        }
        if self.started.is_some() {
            // The byte mark and the report are the same `D`, read twice; the
            // report is preferred only because the daemon parsed it first.
            return self.ended.map(|_| Finished {
                exit: last_exit.or(self.exit_mark),
                ran: true,
            });
        }
        self.reprompted.map(|_| Finished {
            exit: None,
            ran: false,
        })
    }

    /// Whether the shell has begun running the command.
    pub fn started(&self) -> bool {
        self.started.is_some()
    }

    /// The output that belongs to the command, as far as it has got, and how
    /// much of it was dropped from the front to keep it bounded.
    pub fn take(self) -> (Vec<u8>, usize) {
        let all = self.base + self.bytes.len();
        let (from, to) = match (self.started, self.ended, self.reprompted) {
            (Some(start), Some(end), _) => (start, end),
            (Some(start), None, _) => (start, all),
            // Never ran: what the shell printed about the line, up to the new
            // prompt — the echo of the line itself and whatever error it drew.
            (None, _, Some(end)) => (0, end),
            (None, _, None) => (0, all),
        };
        let to = to.clamp(from, all);
        let kept = from.max(self.base).max(to.saturating_sub(KEEP_OUTPUT));
        (
            self.bytes[kept - self.base..to - self.base].to_vec(),
            kept - from,
        )
    }

    /// Bound the memory held: once twice [`KEEP_OUTPUT`] has piled up, drop
    /// all but the last [`KEEP_OUTPUT`]. Only ever what [`take`](Self::take)
    /// would not return anyway, which keeps the newest that much.
    pub fn compact(&mut self) {
        let len = self.bytes.len();
        if len <= KEEP_OUTPUT * 2 {
            return;
        }
        let drop = len - KEEP_OUTPUT;
        self.bytes.drain(..drop);
        self.base += drop;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What zsh with tty7's integration prints for one `echo hi` typed at its
    /// prompt: the echo of the line, `C` with the line, the output, `D` with
    /// the exit code, then the next prompt.
    const ONE_COMMAND: &[u8] = b"echo hi\r\n\x1b]133;C;echo hi\x07hi\r\n\x1b]133;D;0\x07\
                                 \x1b]7;file://host/tmp\x07\x1b]133;A\x07% \x1b]133;B\x07";

    #[test]
    fn the_output_is_what_lies_between_c_and_d() {
        let mut t = Transcript::new();
        t.output(ONE_COMMAND);
        let done = t.prompt(true, Some(0)).expect("D then a prompt is the end");
        assert_eq!((done.exit, done.ran), (Some(0), true));
        assert_eq!(t.take(), (b"hi\r\n".to_vec(), 0));
    }

    #[test]
    fn marks_split_across_reads_still_bound_the_output() {
        // The pty hands the stream over in whatever chunks it likes, including
        // through the middle of a mark.
        for split in 1..ONE_COMMAND.len() {
            let mut t = Transcript::new();
            t.output(&ONE_COMMAND[..split]);
            t.output(&ONE_COMMAND[split..]);
            assert!(t.prompt(true, Some(0)).is_some(), "split at {split}");
            assert_eq!(t.take().0, b"hi\r\n".to_vec(), "split at {split}");
        }
    }

    #[test]
    fn a_prompt_report_before_the_command_has_ended_is_not_the_end() {
        let mut t = Transcript::new();
        // The shell has only echoed the line; a prompt redraw (a `B` in PS1
        // repainted by a transient-prompt theme) reports "at a prompt" too.
        t.output(b"make\r\n\x1b]133;B\x07");
        assert!(t.prompt(true, None).is_none());
        // Running: the daemon reports "not at a prompt".
        t.output(b"\x1b]133;C;make\x07building\r\n");
        assert!(t.prompt(false, None).is_none());
        // A `D` the daemon did not believe — relayed by something running in
        // the foreground, reported as "not at a prompt" — ends nothing.
        t.output(b"\x1b]133;D;7\x07more\r\n");
        assert!(t.prompt(false, Some(7)).is_none());
        t.output(b"done\r\n\x1b]133;D;2\x07\x1b]133;A\x07$ ");
        let done = t.prompt(true, Some(2)).expect("the real end");
        assert_eq!(done.exit, Some(2));
        assert_eq!(
            t.take().0,
            b"building\r\n\x1b]133;D;7\x07more\r\ndone\r\n".to_vec(),
            "the output runs to the D the shell's own prompt followed"
        );
    }

    #[test]
    fn a_prompt_with_no_command_before_it_means_the_line_never_ran() {
        let mut t = Transcript::new();
        t.output(b"echo (\r\nzsh: parse error near `('\r\n\x1b]133;A\x07% ");
        let done = t.prompt(true, None).expect("the shell is back");
        assert_eq!((done.exit, done.ran), (None, false));
        assert_eq!(
            t.take().0,
            b"echo (\r\nzsh: parse error near `('\r\n".to_vec(),
            "what the shell said about the line is the only output there is"
        );
    }

    #[test]
    fn the_exit_code_falls_back_to_the_mark_itself() {
        let mut t = Transcript::new();
        t.output(b"\x1b]133;C\x07\x1b]133;D;5\x1b\\");
        let done = t
            .prompt(true, None)
            .expect("ST terminates a mark as well as BEL");
        assert_eq!(done.exit, Some(5));
        assert!(t.take().0.is_empty());
    }

    #[test]
    fn a_timeout_keeps_what_the_command_has_printed_so_far() {
        let mut t = Transcript::new();
        t.output(b"sleep 9\r\n\x1b]133;C;sleep 9\x07tick\r\n");
        assert_eq!(t.take().0, b"tick\r\n".to_vec());
    }

    #[test]
    fn output_past_the_bound_is_dropped_from_the_front() {
        let mut t = Transcript::new();
        t.output(b"\x1b]133;C\x07");
        let chunk = vec![b'x'; KEEP_OUTPUT];
        for _ in 0..3 {
            t.output(&chunk);
            t.compact();
        }
        t.output(b"end\x1b]133;D;0\x07");
        assert!(t.prompt(true, Some(0)).is_some());
        let (out, dropped) = t.take();
        assert_eq!(out.len(), KEEP_OUTPUT);
        assert!(out.ends_with(b"end"));
        assert_eq!(out.len() + dropped, KEEP_OUTPUT * 3 + 3);
    }
}

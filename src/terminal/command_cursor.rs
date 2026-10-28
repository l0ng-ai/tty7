//! Hands the cursor shape back to the prompt when a command exits (#837).
//!
//! A program that restyles the cursor (DECSCUSR) is supposed to undo it on the
//! way out, and "undo" is whatever `Se` says in its terminfo. We advertise
//! `xterm-256color`, whose `Se` is `\E[2 q` — an explicit *steady block*,
//! because that is xterm's own default — and Neovim sends exactly that on exit
//! to a terminal it takes for xterm (vim goes through the same `Se`). The
//! emulator can only take `2 q` at its word, so every prompt after `nvim` has a
//! block cursor no matter what `cursor_style` says. `\e[0 q`, the one reset
//! that means "the terminal's default", already lands on `cursor_style`: that
//! is `Config::default_cursor_style`, which the emulator falls back to.
//!
//! The shell marks tell us when the program's claim on the cursor ends: `C` is
//! a command starting, `D` is it finishing. [`CommandCursorStyle`] notes the
//! style the prompt had at `C` and, if the command left a different one behind
//! at `D`, puts the prompt's back. `D` is the first thing our precmd writes, so
//! a shell hook that styles its own cursor (a vi-mode plugin, a `precmd` echo)
//! still has the last word.

use tty59_vt::VtTermSession;
use tty59_vt::model::CursorStyle;

/// The two shell-integration marks that bracket a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandMark {
    /// OSC 133;C — the command line was accepted and is about to run.
    Started,
    /// OSC 133;D — it finished and the shell is back.
    Finished,
}

impl CommandMark {
    /// Reads an OSC payload (`133;C;cargo build`, `133;D;0`, …).
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let mark = payload.strip_prefix(b"133;")?;
        let (&kind, rest) = mark.split_first()?;
        if !(rest.is_empty() || rest.first() == Some(&b';')) {
            return None;
        }
        match kind {
            b'C' => Some(Self::Started),
            b'D' => Some(Self::Finished),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct CommandCursorStyle {
    /// The style in effect when the running command started.
    before: Option<CursorStyle>,
}

impl CommandCursorStyle {
    pub fn apply(&mut self, term: &mut VtTermSession<'_>, mark: CommandMark) {
        match mark {
            CommandMark::Started => self.before = Some(term.cursor_style()),
            CommandMark::Finished => {
                let Some(before) = self.before.take() else {
                    return;
                };
                if term.cursor_style() == before {
                    return;
                }
                // Back to "no program has asked for anything" first, so a
                // prompt that was on the configured default stays on it —
                // and keeps following `cursor_style` when that is changed
                // later. Only a prompt that had a style of its own gets that
                // style pinned back.
                term.set_cursor_style(None);
                if term.cursor_style() != before {
                    term.set_cursor_style(Some(before));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::osc::OscTokenizer;
    use tty59_vt::model::CursorShape;

    /// Drives a stream through the emulator the way the pty reader does —
    /// advance to each mark, act on it, carry on — for a pane whose configured
    /// cursor is `default`, and reports the shape it ends on.
    fn shape_after(default: CursorShape, stream: &[u8]) -> CursorShape {
        let mut term = tty59_vt::VtTerm::with_cursor_default(
            CursorStyle { shape: default, blinking: false },
            80,
            24,
            10_000,
        );
        let mut tok = OscTokenizer::new(&[b"133"]);
        let mut restore = CommandCursorStyle::default();

        let mut cuts = Vec::new();
        tok.feed_at(stream, |off, payload| {
            if let Some(mark) = CommandMark::parse(payload) {
                cuts.push((off, mark));
            }
        });
        let mut at = 0;
        for (off, mark) in cuts {
            term.feed(&stream[at..off]);
            at = off;
            restore.apply(&mut term.session(), mark);
        }
        term.feed(&stream[at..]);
        term.session().cursor_style().shape
    }

    #[test]
    fn marks_parse_with_and_without_a_payload() {
        assert_eq!(CommandMark::parse(b"133;C"), Some(CommandMark::Started));
        assert_eq!(
            CommandMark::parse(b"133;C;nvim x"),
            Some(CommandMark::Started)
        );
        assert_eq!(CommandMark::parse(b"133;D;0"), Some(CommandMark::Finished));
        assert_eq!(CommandMark::parse(b"133;D"), Some(CommandMark::Finished));
        assert_eq!(CommandMark::parse(b"133;A"), None);
        assert_eq!(CommandMark::parse(b"133;CX"), None);
        assert_eq!(CommandMark::parse(b"7;file:///"), None);
    }

    /// The #837 repro: nvim's exit sequence under `xterm-256color` is `2 q`.
    #[test]
    fn a_block_left_by_an_exiting_editor_goes_back_to_the_configured_bar() {
        assert_eq!(
            shape_after(
                CursorShape::Beam,
                b"\x1b]133;C;nvim x\x07\x1b[2 q\x1b[?1049h\x1b[?1049l\x1b[2 q\x1b]133;D;0\x07$ "
            ),
            CursorShape::Beam
        );
        assert_eq!(
            shape_after(
                CursorShape::Underline,
                b"\x1b]133;C\x07\x1b[2 q\x1b]133;D;0\x07"
            ),
            CursorShape::Underline
        );
    }

    #[test]
    fn a_style_the_prompt_set_for_itself_is_what_comes_back() {
        assert_eq!(
            shape_after(
                CursorShape::Block,
                b"\x1b[4 q$ \x1b]133;C\x07\x1b[6 q\x1b[2 q\x1b]133;D;0\x07"
            ),
            CursorShape::Underline
        );
    }

    #[test]
    fn a_hook_after_the_command_mark_has_the_last_word() {
        assert_eq!(
            shape_after(
                CursorShape::Block,
                b"\x1b]133;C\x07\x1b[2 q\x1b]133;D;0\x07\x1b[6 q\x1b]133;A\x07$ "
            ),
            CursorShape::Beam
        );
    }

    /// Outside a command nothing is restored: a prompt that restyles its own
    /// cursor between marks keeps what it chose.
    #[test]
    fn a_style_set_at_the_prompt_is_left_alone() {
        assert_eq!(
            shape_after(
                CursorShape::Beam,
                b"\x1b]133;C\x07\x1b]133;D;0\x07\x1b[2 q$ "
            ),
            CursorShape::Block
        );
    }
}

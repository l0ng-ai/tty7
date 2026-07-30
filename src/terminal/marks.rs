use std::sync::{Arc, Mutex};

const MAX_MARKS: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandMark {
    pub row: i64,
    pub text: String,
    pub exit: Option<i32>,
    pub done: bool,
}

#[derive(Clone, Default)]
pub struct Marks(Arc<Mutex<Vec<CommandMark>>>);

impl Marks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(&self, row: i64, text: String) {
        let Ok(mut marks) = self.0.lock() else { return };
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
        let overflow = marks.len().saturating_sub(MAX_MARKS);
        if overflow > 0 {
            marks.drain(..overflow);
        }
    }

    pub fn set_text(&self, text: String) {
        let Ok(mut marks) = self.0.lock() else { return };
        if let Some(last) = marks.last_mut() {
            if !last.done {
                last.text = text;
            }
        }
    }

    pub fn finish(&self, exit: Option<i32>) {
        let Ok(mut marks) = self.0.lock() else { return };
        if let Some(last) = marks.last_mut() {
            last.done = true;
            last.exit = exit;
        }
    }

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

    pub fn clear(&self) {
        if let Ok(mut marks) = self.0.lock() {
            marks.clear();
        }
    }
}

pub fn parse_done_exit(payload: &[u8]) -> Option<i32> {
    let rest = payload.strip_prefix(b"D")?;
    let rest = rest.strip_prefix(b";")?;
    std::str::from_utf8(rest).ok()?.trim().parse().ok()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkEvent {
    Prompt,
    Command(String),
    Done(Option<i32>),
}

#[derive(Default)]
pub struct MarkScanner {
    state: ScanState,
    payload: Vec<u8>,
}

#[derive(Default, PartialEq, Eq)]
enum ScanState {
    #[default]
    Text,
    Esc,
    Osc,
    OscEsc,
}

const MAX_PAYLOAD: usize = 64 * 1024;

impl MarkScanner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8], mut on_mark: impl FnMut(usize, MarkEvent)) {
        let mut i = 0;
        while i < bytes.len() {
            if self.state == ScanState::Text {
                let Some(off) = memchr::memchr(0x1b, &bytes[i..]) else {
                    return;
                };
                self.state = ScanState::Esc;
                i += off + 1;
                continue;
            }
            let b = bytes[i];
            match self.state {
                ScanState::Text => unreachable!(),
                ScanState::Esc => {
                    if b == b']' {
                        self.state = ScanState::Osc;
                        self.payload.clear();
                    } else {
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

    fn take(&mut self) -> Option<MarkEvent> {
        let payload = std::mem::take(&mut self.payload);
        let body = payload.strip_prefix(b"133;")?;
        match body.first()? {
            b'A' => Some(MarkEvent::Prompt),
            b'C' => {
                let cmd = body
                    .strip_prefix(b"C;")
                    .map(|c| String::from_utf8_lossy(c).into_owned())
                    .unwrap_or_default();
                Some(MarkEvent::Command(cmd))
            }
            b'D' => Some(MarkEvent::Done(parse_done_exit(body))),
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

    fn scan(chunks: &[&[u8]]) -> Vec<(usize, MarkEvent)> {
        let mut scanner = MarkScanner::new();
        let mut out = Vec::new();
        for chunk in chunks {
            scanner.feed(chunk, |off, ev| out.push((off, ev)));
        }
        out
    }

    #[test]
    fn splitting_anywhere_yields_the_same_marks() {
        let stream: &[u8] =
            b"out\x1b\x1b[32mmore\x1b]133;C;git status\x07text\x1b]133;D;0\x1b\\tail\x1b";
        let whole = scan(&[stream]);
        assert_eq!(whole.len(), 2, "both marks found in one pass");

        for at in 0..=stream.len() {
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

use base64::Engine as _;

const MAX_PAYLOAD: usize = 8192;

pub struct OscTokenizer {
    ids: &'static [&'static [u8]],
    buf: Vec<u8>,
    state: State,
}

#[derive(Default, Clone, Copy)]
enum State {
    #[default]
    Ground,
    Esc,
    Osc,
    OscEsc,
    Ignore,
    IgnoreEsc,
}

impl OscTokenizer {
    pub fn new(ids: &'static [&'static [u8]]) -> Self {
        Self {
            ids,
            buf: Vec::new(),
            state: State::Ground,
        }
    }

    pub fn feed(&mut self, bytes: &[u8], mut on_payload: impl FnMut(&[u8])) {
        self.feed_at(bytes, |_, payload| on_payload(payload));
    }

    /// [`feed`](Self::feed), but also reporting where each payload ended: an
    /// offset one past its terminator, in ascending order, so a reader can
    /// advance an emulator to exactly there and read the state the sequence
    /// left behind. A payload split across two feeds is reported against the
    /// batch its terminator landed in.
    pub fn feed_at(&mut self, bytes: &[u8], mut on_payload: impl FnMut(usize, &[u8])) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => {
                    let Some(off) = memchr::memchr(0x1b, &bytes[i..]) else {
                        return;
                    };
                    self.state = State::Esc;
                    i += off + 1;
                    continue;
                }
                State::Ignore => {
                    let Some(off) = memchr::memchr2(0x07, 0x1b, &bytes[i..]) else {
                        return;
                    };
                    self.state = if bytes[i + off] == 0x07 {
                        State::Ground
                    } else {
                        State::IgnoreEsc
                    };
                    i += off + 1;
                    continue;
                }
                _ => {}
            }
            let b = bytes[i];
            match self.state {
                State::Ground | State::Ignore => unreachable!(),
                State::Esc => match b {
                    b']' => {
                        self.buf.clear();
                        self.state = State::Osc;
                    }
                    0x1b => {}
                    _ => self.state = State::Ground,
                },
                State::Osc => match b {
                    0x07 => self.finish(i + 1, &mut on_payload),
                    0x1b => self.state = State::OscEsc,
                    _ => {
                        self.buf.push(b);
                        if self.buf.len() > MAX_PAYLOAD || !self.identifier_could_match() {
                            self.buf.clear();
                            self.state = State::Ignore;
                        }
                    }
                },
                State::OscEsc => match b {
                    b'\\' => self.finish(i + 1, &mut on_payload),
                    0x1b => {}
                    b']' => {
                        self.buf.clear();
                        self.state = State::Osc;
                    }
                    _ => {
                        self.buf.clear();
                        self.state = State::Ground;
                    }
                },
                State::IgnoreEsc => match b {
                    b'\\' => self.state = State::Ground,
                    0x1b => {}
                    b']' => {
                        self.buf.clear();
                        self.state = State::Osc;
                    }
                    _ => self.state = State::Ground,
                },
            }
            i += 1;
        }
    }

    fn identifier_could_match(&self) -> bool {
        match self.buf.iter().position(|&b| b == b';') {
            Some(pos) => self.ids.iter().any(|&id| id == &self.buf[..pos]),
            None => self.ids.iter().any(|id| id.starts_with(&self.buf)),
        }
    }

    fn finish(&mut self, at: usize, on_payload: &mut impl FnMut(usize, &[u8])) {
        on_payload(at, &self.buf);
        self.buf.clear();
        self.state = State::Ground;
    }
}

/// A desktop notification: its title, if it has one, and its body.
pub type Note = (Option<String>, String);

pub fn parse_notification(payload: &[u8]) -> Option<Note> {
    if let Some(rest) = payload.strip_prefix(b"9;") {
        let first = rest.split(|&b| b == b';').next().unwrap_or(rest);
        // ConEmu's subcommands, 9;1 through 9;12: progress, cwd, prompt marks.
        if (first.len() == 1 && first[0].is_ascii_digit()) || matches!(first, b"10" | b"11" | b"12")
        {
            return None;
        }
        let body = String::from_utf8_lossy(rest).into_owned();
        return (!body.is_empty()).then_some((None, body));
    }
    if let Some(rest) = payload.strip_prefix(b"777;notify;") {
        let mut parts = rest.splitn(2, |&b| b == b';');
        let first = String::from_utf8_lossy(parts.next().unwrap_or(b"")).into_owned();
        let second = parts
            .next()
            .map(|b| String::from_utf8_lossy(b).into_owned());
        let (title, body) = match second {
            Some(body) if !body.is_empty() => (Some(first), body),
            _ => (None, first),
        };
        return (!body.is_empty()).then_some((title, body));
    }
    None
}

/// Desktop notifications across OSC 9, 99 and 777. OSC 99 is kitty's
/// protocol, which is stateful: a title and a body can arrive as separate
/// chunks sharing an `i=` id, and the notification is complete at the first
/// chunk without `d=0`. <https://sw.kovidgoyal.net/kitty/desktop-notifications/>
#[derive(Default)]
pub struct Notifications {
    /// Unfinished OSC 99 notifications.
    kitty: Vec<PendingKitty>,
}

#[derive(Default)]
struct PendingKitty {
    id: String,
    title: String,
    body: String,
}

/// Unfinished kitty notifications kept at once; a program that opens ids and
/// never finishes them loses the oldest.
const MAX_PENDING_KITTY: usize = 8;

impl Notifications {
    /// Reads one OSC payload, identifier included, and returns the
    /// notification it completes, as `(title, body)`.
    pub fn parse(&mut self, payload: &[u8]) -> Option<Note> {
        match payload.strip_prefix(b"99;") {
            Some(rest) => self.kitty(rest),
            None => parse_notification(payload),
        }
    }

    fn kitty(&mut self, rest: &[u8]) -> Option<Note> {
        let split = rest.iter().position(|&b| b == b';');
        let (meta, data) = match split {
            Some(at) => (&rest[..at], &rest[at + 1..]),
            None => (rest, &b""[..]),
        };
        let meta = String::from_utf8_lossy(meta);
        let (mut id, mut done, mut part, mut b64) = ("", true, "title", false);
        for kv in meta.split(':') {
            match kv.split_once('=') {
                Some(("i", v)) => id = v,
                Some(("d", v)) => done = v != "0",
                Some(("p", v)) => part = v,
                Some(("e", v)) => b64 = v == "1",
                _ => {}
            }
        }
        // A close, a liveness or capability query: commands about
        // notifications, not parts of one, so they neither show nor finish one.
        if matches!(part, "close" | "alive" | "?") {
            return None;
        }
        // Chunks without an id are each their own notification; one never
        // continues another.
        if id.is_empty() {
            self.kitty.retain(|p| !p.id.is_empty());
        }
        let text = match b64 {
            true => base64::engine::general_purpose::STANDARD
                .decode(data)
                .map(|d| String::from_utf8_lossy(&d).into_owned())
                .unwrap_or_default(),
            false => String::from_utf8_lossy(data).into_owned(),
        };
        let at = match self.kitty.iter().position(|p| p.id == id) {
            Some(at) => at,
            None => {
                if self.kitty.len() == MAX_PENDING_KITTY {
                    self.kitty.remove(0);
                }
                self.kitty.push(PendingKitty {
                    id: id.to_string(),
                    ..Default::default()
                });
                self.kitty.len() - 1
            }
        };
        let pending = &mut self.kitty[at];
        let field = match part {
            "title" => Some(&mut pending.title),
            "body" => Some(&mut pending.body),
            _ => None,
        };
        if let Some(field) = field
            && field.len() + text.len() <= MAX_PAYLOAD
        {
            field.push_str(&text);
        }
        if !done {
            return None;
        }
        let PendingKitty { title, body, .. } = self.kitty.remove(at);
        match (title.is_empty(), body.is_empty()) {
            (true, true) => None,
            (false, true) => Some((None, title)),
            (true, false) => Some((None, body)),
            (false, false) => Some((Some(title), body)),
        }
    }
}

/// How many of a pane's notes reach the desktop per [`NOTE_WINDOW`].
pub const NOTES_PER_WINDOW: usize = 5;
pub const NOTE_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// A pane's notification rate limit. Pane output is untrusted, and capping
/// the queue between polls still lets a flood through a few notes per poll.
/// At most [`NOTES_PER_WINDOW`] notes are shown per [`NOTE_WINDOW`]; the rest
/// are dropped and said as one note when the window turns over, which takes
/// a slot. A tumbling window: a burst straddling its edge can show twice the
/// limit in quick succession.
#[derive(Default)]
pub struct NoteBudget {
    since: Option<std::time::Instant>,
    shown: usize,
    dropped: usize,
}

impl NoteBudget {
    /// Of `wanted` notes at `now`: how many to show, and whether notes were
    /// dropped earlier and are now due to be said as one.
    pub fn admit(&mut self, now: std::time::Instant, wanted: usize) -> (usize, bool) {
        let mut due = false;
        if self
            .since
            .is_none_or(|s| now.duration_since(s) >= NOTE_WINDOW)
        {
            due = std::mem::take(&mut self.dropped) > 0;
            self.since = Some(now);
            self.shown = usize::from(due);
        }
        let show = wanted.min(NOTES_PER_WINDOW.saturating_sub(self.shown));
        self.shown += show;
        self.dropped += wanted - show;
        (show, due)
    }
}

/// What a sequence did to the title a pane is showing — see
/// [`TitleLifetime`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TitleEffect {
    /// Nothing to do with the title.
    None,
    /// An OSC 0/2: the pane just named itself.
    Set,
    /// The command that set the title showing has finished, so the title
    /// describes something that is no longer running and has to go.
    Retire,
}

/// How long a title a program set outlives the program (#889).
///
/// An OSC 0/2 has no owner and no end: whatever a pane last named itself
/// stands until something else names it. That is right while the program that
/// wrote it is still running, and wrong the instant it exits — a tab that goes
/// on reading "✳ refactoring the parser" after Claude Code has quit is naming
/// a session that no longer exists, and the pane's directory (the rung below
/// it in the label ladder) would describe it far better.
///
/// The shell integration already says when a command starts and stops: OSC
/// 133;C and 133;D. So a title set *between* them belongs to that command and
/// is retired by its `D`; a title set at a prompt — the shell's own, or one
/// the reader pinned by hand with a bare `printf '\e]0;…'` — belongs to
/// nobody in particular and is left alone. A pane with no shell integration
/// sees neither mark and so keeps every title, exactly as before.
///
/// Both the daemon (which keeps `PaneRecord::osc_title` for the switcher and
/// the CLI) and the window (which keeps its own terminal's title for its tab
/// strip) run this over the same bytes, so the two can never disagree about
/// whether a title is still current. Feeding it in stream order is what makes
/// the answer right: a shell that re-titles itself in `precmd` emits its OSC
/// 0/2 *after* the `D`, and that [`Set`](TitleEffect::Set) is simply the last
/// word.
#[derive(Debug, Default, Clone, Copy)]
pub struct TitleLifetime {
    /// A command owns the pane: a `C` has arrived and its `D` has not.
    running: bool,
    /// The title standing right now was set while a command owned the pane.
    from_command: bool,
    /// Any `133` prompt mark (`A`–`D`) has been read at all.
    marked: bool,
}

impl TitleLifetime {
    /// Reads one OSC payload — identifier included, as
    /// [`OscTokenizer`] reports it — and says what it did to the title.
    pub fn saw(&mut self, payload: &[u8]) -> TitleEffect {
        if payload.starts_with(b"0;") || payload.starts_with(b"2;") {
            self.from_command = self.running;
            return TitleEffect::Set;
        }
        let Some(rest) = payload.strip_prefix(b"133;") else {
            return TitleEffect::None;
        };
        if matches!(rest.first(), Some(b'A'..=b'D')) {
            self.marked = true;
        }
        match rest.first() {
            Some(b'C') => self.running = true,
            Some(b'D') => {
                let retire = self.running && self.from_command;
                // Whatever stands after this mark was not written by a
                // command that is still running, whoever wrote it.
                self.running = false;
                self.from_command = false;
                if retire {
                    return TitleEffect::Retire;
                }
            }
            // `A` and `B` only draw a prompt. They must not retire anything:
            // a shell's own `precmd` title lands between the `D` and the `A`.
            _ => {}
        }
        TitleEffect::None
    }

    /// The stream was picked up partway through a command whose `C` mark is
    /// not in it — a window reattaching to a pane whose replay ring rolled
    /// past the `C` while a long session (an agent, an editor) ran on.
    ///
    /// If what was read carried no prompt mark at all, every byte of it was
    /// written under that command, titles included, so the command's `D`
    /// must retire them just as it would have on a link that saw the `C`.
    /// Without this a reattached window keeps the dead program's title
    /// forever — exactly #889 — while the daemon, which saw the whole stream,
    /// has already dropped it. Any mark read settles the question on its own,
    /// so this does nothing then.
    pub fn joined_mid_command(&mut self) {
        if !self.marked {
            self.running = true;
            self.from_command = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_flood_shows_a_few_per_window_and_one_note_for_the_rest() {
        let mut budget = NoteBudget::default();
        let t0 = Instant::now();
        // Three notes a poll, every 300ms, for ten seconds.
        let mut shown = 0;
        for poll in 0..33 {
            let (show, due) = budget.admit(t0 + Duration::from_millis(300 * poll), 3);
            shown += show;
            assert!(!due);
        }
        assert_eq!(shown, NOTES_PER_WINDOW);
        assert_eq!(budget.admit(t0 + NOTE_WINDOW, 3), (3, true), "said once");
        assert_eq!(
            budget.admit(t0 + NOTE_WINDOW, 3),
            (NOTES_PER_WINDOW - 4, false),
            "the note took a slot"
        );
        assert_eq!(
            budget.admit(t0 + NOTE_WINDOW * 2, 0),
            (0, true),
            "said even when the flood stopped"
        );
        assert_eq!(budget.admit(t0 + NOTE_WINDOW * 3, 0), (0, false));
    }

    fn collect(ids: &'static [&'static [u8]], chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut tok = OscTokenizer::new(ids);
        let mut out = Vec::new();
        for c in chunks {
            tok.feed(c, |payload| out.push(payload.to_vec()));
        }
        out
    }

    #[test]
    fn bel_and_st_terminators_both_complete_a_payload() {
        assert_eq!(
            collect(&[b"9"], &[b"\x1b]9;bel\x07"]),
            vec![b"9;bel".to_vec()]
        );
        assert_eq!(
            collect(&[b"9"], &[b"\x1b]9;st\x1b\\"]),
            vec![b"9;st".to_vec()]
        );
    }

    #[test]
    fn sequence_split_across_reads_is_reassembled() {
        assert_eq!(
            collect(&[b"7"], &[b"\x1b]7;file:", b"//h/x", b"\x07"]),
            vec![b"7;file://h/x".to_vec()]
        );
        assert_eq!(
            collect(&[b"9"], &[b"\x1b]9;ping\x1b", b"\\"]),
            vec![b"9;ping".to_vec()]
        );
    }

    #[test]
    fn uninteresting_identifiers_are_skipped_and_state_recovers() {
        assert_eq!(
            collect(
                &[b"9"],
                &[b"\x1b]0;title\x07\x1b]52;c;abc\x1b\\\x1b]9;kept\x07"]
            ),
            vec![b"9;kept".to_vec()]
        );
    }

    #[test]
    fn resyncs_on_new_osc_after_an_unterminated_one() {
        assert_eq!(
            collect(&[b"9"], &[b"\x1b]9;dropped\x1b]9;kept\x07"]),
            vec![b"9;kept".to_vec()]
        );
        assert_eq!(
            collect(&[b"9"], &[b"\x1b]0;title\x1b]9;kept\x07"]),
            vec![b"9;kept".to_vec()]
        );
    }

    #[test]
    fn identifier_prefix_matching_buffers_only_possible_ids() {
        let ids: &'static [&'static [u8]] = &[b"777"];
        assert_eq!(
            collect(ids, &[b"\x1b]78;x\x07\x1b]777;y\x07"]),
            vec![b"777;y".to_vec()]
        );
        assert_eq!(collect(ids, &[b"\x1b]77;x\x07"]), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn oversized_payload_is_abandoned_not_truncated() {
        let mut big = b"\x1b]9;".to_vec();
        big.extend(std::iter::repeat_n(b'x', MAX_PAYLOAD + 1));
        big.extend_from_slice(b"\x07\x1b]9;next\x07");
        assert_eq!(collect(&[b"9"], &[&big]), vec![b"9;next".to_vec()]);
    }

    #[test]
    fn byte_at_a_time_delivery_reassembles_every_state_transition() {
        let stream = b"\x1b]0;title\x07\x1b]133;A\x1b\\plain\x1b]7;file://h/x\x07";
        let chunks: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(
            collect(&[b"7", b"133"], &chunks),
            vec![b"133;A".to_vec(), b"7;file://h/x".to_vec()]
        );
    }

    #[test]
    fn ignored_sequence_split_across_reads_still_recovers() {
        assert_eq!(
            collect(
                &[b"9"],
                &[b"\x1b]52;c;abc", b"defgh\x1b", b"\\\x1b]9;ok\x07"]
            ),
            vec![b"9;ok".to_vec()]
        );
    }

    #[test]
    fn offsets_land_one_past_the_terminator() {
        let mut tok = OscTokenizer::new(&[b"9"]);
        let mut got = Vec::new();
        let stream = b"ab\x1b]9;bel\x07cd\x1b]9;st\x1b\\";
        tok.feed_at(stream, |at, payload| got.push((at, payload.to_vec())));
        assert_eq!(
            got,
            vec![(10, b"9;bel".to_vec()), (20, b"9;st".to_vec())],
            "a cut must point just past its sequence"
        );
        assert_eq!(&stream[10..12], b"cd");
        assert_eq!(stream.len(), 20, "the ST-terminated one ends the stream");
    }

    #[test]
    fn an_offset_is_reported_against_the_batch_its_terminator_lands_in() {
        let mut tok = OscTokenizer::new(&[b"777"]);
        let mut got = Vec::new();
        tok.feed_at(b"out\x1b]777;no", |at, p| got.push((at, p.to_vec())));
        assert!(got.is_empty(), "unterminated, so nothing to report yet");
        tok.feed_at(b"tify;x\x07tail", |at, p| got.push((at, p.to_vec())));
        assert_eq!(got, vec![(7, b"777;notify;x".to_vec())]);
    }

    /// Feeds a stream through the tokenizer the way both readers do and
    /// reports what the title ended up being: `Some(t)` for a title that
    /// stands, `None` for one that was retired or never set.
    fn showing(stream: &[u8]) -> Option<String> {
        let mut tok = OscTokenizer::new(&[b"0", b"2", b"133"]);
        let mut life = TitleLifetime::default();
        let mut title = None;
        tok.feed(stream, |payload| match life.saw(payload) {
            TitleEffect::Set => {
                let body = payload.split(|&b| b == b';').nth(1).unwrap_or(b"");
                title = (!body.is_empty()).then(|| String::from_utf8_lossy(body).into_owned());
            }
            TitleEffect::Retire => title = None,
            TitleEffect::None => {}
        });
        title
    }

    /// The bug in #889: Claude Code names the tab, quits, and the name stays
    /// on a pane that is back at its own prompt in a real directory.
    #[test]
    fn a_title_a_command_set_is_retired_when_that_command_finishes() {
        assert_eq!(
            showing(b"\x1b]133;C;claude\x07\x1b]0;refactoring the parser\x07\x1b]133;D;0\x07"),
            None,
            "the program that named the tab has exited"
        );
    }

    /// The case the title is *supposed* to stick for: a TUI that names itself
    /// once and keeps running.
    #[test]
    fn a_title_of_a_command_still_running_stands() {
        assert_eq!(
            showing(b"\x1b]133;C;vim\x07\x1b]0;vim \xe2\x80\x94 main.rs\x07").as_deref(),
            Some("vim — main.rs"),
        );
    }

    /// A shell that re-titles itself in `precmd` writes its OSC 0/2 after the
    /// `D` and before the `A` (tty59's own zsh helper prepends the `D` emitter
    /// for exactly that reason, and the PowerShell one titles between them).
    /// Reading the stream in order is what keeps that title.
    #[test]
    fn a_title_the_shell_writes_at_its_next_prompt_is_the_last_word() {
        assert_eq!(
            showing(
                b"\x1b]133;C;claude\x07\x1b]0;claude\x07\
                  \x1b]133;D;0\x07\x1b]0;me@box:~/dev\x07\x1b]133;A\x07\x1b]133;B\x07"
            )
            .as_deref(),
            Some("me@box:~/dev"),
        );
    }

    /// A title nobody's command set — the shell's, or one pinned by hand at a
    /// prompt — is not a command's to retire.
    #[test]
    fn a_title_set_at_a_prompt_survives_the_next_command() {
        assert_eq!(
            showing(
                b"\x1b]133;A\x07\x1b]0;my tab\x07\x1b]133;B\x07\
                  \x1b]133;C;ls\x07\x1b]133;D;0\x07"
            )
            .as_deref(),
            Some("my tab"),
        );
    }

    /// Without shell integration there are no marks at all, and a title is
    /// kept the way it always was.
    #[test]
    fn a_pane_with_no_marks_keeps_every_title() {
        assert_eq!(showing(b"\x1b]2;anything\x07").as_deref(), Some("anything"));
        let mut life = TitleLifetime::default();
        assert_eq!(life.saw(b"7;file://h/x"), TitleEffect::None);
        assert_eq!(life.saw(b"133;V;1"), TitleEffect::None);
    }

    /// A `D` with no command before it reports the shell's own startup, not a
    /// command that ended; there is nothing of anyone's to retire.
    #[test]
    fn a_d_mark_with_no_command_before_it_retires_nothing() {
        let mut life = TitleLifetime::default();
        assert_eq!(life.saw(b"0;pinned"), TitleEffect::Set);
        assert_eq!(life.saw(b"133;D;0"), TitleEffect::None);
    }

    /// A reattach whose replay ring no longer holds the running command's `C`:
    /// the titles in it are that command's, and its `D` retires them.
    #[test]
    fn a_stream_joined_mid_command_retires_its_title_at_the_d() {
        let mut life = TitleLifetime::default();
        assert_eq!(
            life.saw(b"2;\xe2\x9c\xb3 fixing the switcher"),
            TitleEffect::Set
        );
        life.joined_mid_command();
        assert_eq!(life.saw(b"133;D;0"), TitleEffect::Retire);

        // A replay that carried marks already knows who owns the title: a
        // title pinned at a prompt stays pinned through the next command.
        let mut life = TitleLifetime::default();
        assert_eq!(life.saw(b"133;B"), TitleEffect::None);
        assert_eq!(life.saw(b"0;my tab"), TitleEffect::Set);
        life.joined_mid_command();
        assert_eq!(life.saw(b"133;C;ls"), TitleEffect::None);
        assert_eq!(life.saw(b"133;D;0"), TitleEffect::None);
    }

    #[test]
    fn esc_runs_and_non_osc_escapes_do_not_confuse_the_scanner() {
        assert_eq!(
            collect(&[b"9"], &[b"\x1b\x1b]9;ok\x07"]),
            vec![b"9;ok".to_vec()]
        );
        assert_eq!(
            collect(&[b"9"], &[b"\x1b]9;half\x1b[0m\x1b]9;whole\x07"]),
            vec![b"9;whole".to_vec()]
        );
    }

    fn notes(payloads: &[&[u8]]) -> Vec<Note> {
        let mut n = Notifications::default();
        payloads.iter().filter_map(|p| n.parse(p)).collect()
    }

    #[test]
    fn kitty_notification_in_one_chunk_is_its_title() {
        assert_eq!(
            notes(&[b"99;;Build done"]),
            vec![(None, "Build done".into())]
        );
        assert_eq!(notes(&[b"99;i=1;hi"]), vec![(None, "hi".into())]);
    }

    #[test]
    fn kitty_chunks_sharing_an_id_make_one_notification() {
        // What Claude Code writes for its `kitty` channel.
        assert_eq!(
            notes(&[
                b"99;i=42:d=0:p=title;Claude Code",
                b"99;i=42:p=body;Claude needs your permission",
                b"99;i=42:d=1:a=focus;",
            ]),
            vec![(
                Some("Claude Code".into()),
                "Claude needs your permission".into()
            )]
        );
    }

    #[test]
    fn kitty_chunks_append_and_interleaved_ids_stay_apart() {
        assert_eq!(
            notes(&[
                b"99;i=a:d=0;Hel",
                b"99;i=b:d=0:p=body;other",
                b"99;i=a:d=0;lo",
                b"99;i=a:p=body;world",
            ]),
            vec![(Some("Hello".into()), "world".into())]
        );
    }

    #[test]
    fn kitty_base64_payload_is_decoded() {
        assert_eq!(
            notes(&[b"99;e=1:p=body;aOKAkmxsbw=="]),
            vec![(None, "h\u{2012}llo".into())]
        );
    }

    #[test]
    fn kitty_control_payloads_show_nothing() {
        assert_eq!(notes(&[b"99;i=1:p=close;"]), vec![]);
        assert_eq!(notes(&[b"99;i=1:p=?;"]), vec![]);
    }

    #[test]
    fn kitty_control_payloads_leave_an_unfinished_note_alone() {
        for control in [&b"99;i=1:p=close;"[..], b"99;i=1:p=alive;", b"99;i=1:p=?;"] {
            assert_eq!(notes(&[b"99;i=1:d=0;Half", control]), vec![], "{control:?}");
        }
        assert_eq!(
            notes(&[b"99;i=1:d=0;Hel", b"99;i=1:p=close;", b"99;i=1;lo"]),
            vec![(None, "Hello".into())]
        );
    }

    #[test]
    fn kitty_chunks_without_an_id_never_join() {
        assert_eq!(
            notes(&[b"99;d=0;stale", b"99;p=body;fresh"]),
            vec![(None, "fresh".into())]
        );
    }

    #[test]
    fn osc_9_and_777_still_parse_through_the_same_reader() {
        assert_eq!(
            notes(&[b"9;ping", b"777;notify;T;B", b"9;4;1;50"]),
            vec![(None, "ping".into()), (Some("T".into()), "B".into())]
        );
    }
}

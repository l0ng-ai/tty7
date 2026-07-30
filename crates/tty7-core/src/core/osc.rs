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
                    0x07 => self.finish(&mut on_payload),
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
                    b'\\' => self.finish(&mut on_payload),
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

    fn finish(&mut self, on_payload: &mut impl FnMut(&[u8])) {
        on_payload(&self.buf);
        self.buf.clear();
        self.state = State::Ground;
    }
}

pub fn parse_notification(payload: &[u8]) -> Option<(Option<String>, String)> {
    if let Some(rest) = payload.strip_prefix(b"9;") {
        let first = rest.split(|&b| b == b';').next().unwrap_or(rest);
        if first.len() == 1 && first[0].is_ascii_digit() {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

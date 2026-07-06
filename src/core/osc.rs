//! Streaming OSC (Operating System Command) extractor.
//!
//! The one implementation of OSC wire framing, shared by both byte-stream
//! consumers: the daemon-side cwd/prompt sniffer (`daemon::pane`, OSC 7/133)
//! and the client-side notification scanner (`terminal::remote`, OSC 9/777).
//! The framing rules — `ESC ]` opens, `BEL` or `ESC \` (ST) terminates, a bare
//! `ESC ]` inside an unterminated sequence re-opens a fresh one, oversized
//! payloads are abandoned — are subtle enough that both sites needed the same
//! resync bugfix when each carried its own copy. Keeping the state machine
//! here means a framing change can't silently apply to one consumer and not
//! the other.
//!
//! This is deliberately *not* a full VT parser (the grid has an
//! `ansi::Processor` for that). It tracks just enough state to hand complete
//! payloads of the OSC identifiers a consumer cares about to its callback,
//! bailing out cheaply on any other OSC (e.g. a multi-megabyte OSC 52
//! clipboard write) without buffering it.

/// Cap on how many bytes of a single OSC payload we'll buffer before giving up
/// on it — a guard against an unterminated or absurdly long sequence growing
/// the buffer without bound. Real cwd/prompt/notification payloads are far
/// shorter.
const MAX_PAYLOAD: usize = 8192;

/// A streaming tokenizer for the OSC sequences whose identifiers are listed in
/// `ids`. Feed it raw output bytes; it invokes a callback with each complete
/// payload. State persists across `feed` calls, so a sequence split over
/// multiple reads is still recognized.
pub struct OscTokenizer {
    /// OSC identifiers (the digits before the first `;`) the consumer wants
    /// buffered and delivered; every other OSC is discarded unbuffered.
    ids: &'static [&'static [u8]],
    /// Payload bytes accumulated after `ESC ]` while the identifier can still
    /// match `ids`. Cleared whenever a sequence finishes or is abandoned.
    buf: Vec<u8>,
    state: State,
}

#[derive(Default, Clone, Copy)]
enum State {
    /// Not inside an escape sequence.
    #[default]
    Ground,
    /// Saw `ESC` in ground state; a following `]` opens an OSC.
    Esc,
    /// Inside an OSC whose identifier still matches (a prefix of) `ids`;
    /// buffering the payload.
    Osc,
    /// Saw `ESC` while buffering an OSC — a following `\` is the `ST` terminator.
    OscEsc,
    /// Inside an OSC we've decided to ignore; discard bytes until the terminator.
    Ignore,
    /// Saw `ESC` while ignoring an OSC — a following `\` is the `ST` terminator.
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

    /// Feed one chunk of output; invoke `on_payload` with the complete payload
    /// (identifier included, terminator excluded — e.g. `7;file://…`) of every
    /// interesting OSC that completes within the chunk.
    ///
    /// The tokenizer sits on the full-throughput output stream (both the
    /// daemon's PTY reader and the client's socket reader run it over every
    /// byte), so the two states that dominate real streams — `Ground` between
    /// sequences, `Ignore` inside a discarded OSC (e.g. a multi-MB OSC 52) —
    /// skip ahead with SIMD `memchr` instead of stepping per byte. Everything
    /// else is rare enough to stay a plain per-byte state machine.
    pub fn feed(&mut self, bytes: &[u8], mut on_payload: impl FnMut(&[u8])) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => {
                    // Nothing before the next ESC can matter.
                    let Some(off) = memchr::memchr(0x1b, &bytes[i..]) else {
                        return;
                    };
                    self.state = State::Esc;
                    i += off + 1;
                    continue;
                }
                State::Ignore => {
                    // Only BEL (terminates) or ESC (may terminate or fork) can
                    // end a discarded payload.
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
                // Handled by the skip-ahead arms above.
                State::Ground | State::Ignore => unreachable!(),
                State::Esc => match b {
                    b']' => {
                        self.buf.clear();
                        self.state = State::Osc;
                    }
                    0x1b => {} // a run of ESCs; keep waiting for the next byte
                    _ => self.state = State::Ground,
                },
                State::Osc => match b {
                    0x07 => self.finish(&mut on_payload), // BEL terminator
                    0x1b => self.state = State::OscEsc,
                    _ => {
                        self.buf.push(b);
                        // Abandon as soon as the identifier can't be one of
                        // `ids`, or the payload grows unreasonably large.
                        if self.buf.len() > MAX_PAYLOAD || !self.identifier_could_match() {
                            self.buf.clear();
                            self.state = State::Ignore;
                        }
                    }
                },
                State::OscEsc => match b {
                    b'\\' => self.finish(&mut on_payload), // ST terminator
                    0x1b => {}                             // another ESC: stay poised for the `\`
                    // The ESC began a *new* OSC, aborting this unterminated one.
                    // Re-open a fresh OSC instead of dropping the `]` into
                    // Ground — otherwise a well-formed sequence directly
                    // following an unterminated one would be silently lost.
                    b']' => {
                        self.buf.clear();
                        self.state = State::Osc;
                    }
                    _ => {
                        // ESC began some other (non-OSC) escape: abandon this OSC.
                        self.buf.clear();
                        self.state = State::Ground;
                    }
                },
                State::IgnoreEsc => match b {
                    b'\\' => self.state = State::Ground,
                    0x1b => {} // stay, another ESC
                    // Same resync as `OscEsc`: the ESC opened a new OSC — scan
                    // it rather than missing the sequence that follows an
                    // unterminated, ignored one (e.g. a title OSC).
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

    /// Whether the identifier accumulated so far can still become one of `ids`.
    /// Before the first `;` it is a prefix being built up; once the `;` arrives
    /// it must match exactly.
    fn identifier_could_match(&self) -> bool {
        match self.buf.iter().position(|&b| b == b';') {
            Some(pos) => self.ids.iter().any(|&id| id == &self.buf[..pos]),
            None => self.ids.iter().any(|id| id.starts_with(&self.buf)),
        }
    }

    /// A complete, interesting OSC payload arrived: hand it to the consumer.
    fn finish(&mut self, on_payload: &mut impl FnMut(&[u8])) {
        on_payload(&self.buf);
        self.buf.clear();
        self.state = State::Ground;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a tokenizer for `ids` over the chunks and collect delivered payloads.
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
        // Torn mid-payload and between the ESC and its ST backslash.
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
        // OSC 0 (title) and OSC 52 (clipboard) are not in `ids`: nothing is
        // delivered, and an interesting OSC right after is still caught.
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
        // Regression (fixed independently in both pre-extraction copies): the
        // ESC that aborts an unterminated OSC may itself open the next one; the
        // `]` must re-open a fresh OSC rather than fall into Ground. Covers
        // both the buffering path and the ignore path.
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
        // `77` is a prefix of `777` but `78` can no longer match: only the
        // former's completed sequence is delivered.
        let ids: &'static [&'static [u8]] = &[b"777"];
        assert_eq!(
            collect(ids, &[b"\x1b]78;x\x07\x1b]777;y\x07"]),
            vec![b"777;y".to_vec()]
        );
        // After the `;` the identifier must match exactly: `77;` is not `777`.
        assert_eq!(collect(ids, &[b"\x1b]77;x\x07"]), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn oversized_payload_is_abandoned_not_truncated() {
        // A payload past the cap is dropped entirely (delivering a truncated
        // cwd or notification would be worse than delivering none), and the
        // stream recovers for the next sequence.
        let mut big = b"\x1b]9;".to_vec();
        big.extend(std::iter::repeat_n(b'x', MAX_PAYLOAD + 1));
        big.extend_from_slice(b"\x07\x1b]9;next\x07");
        assert_eq!(collect(&[b"9"], &[&big]), vec![b"9;next".to_vec()]);
    }

    #[test]
    fn byte_at_a_time_delivery_reassembles_every_state_transition() {
        // The harshest tearing: one byte per `feed` call, crossing every state
        // boundary (ESC/], identifier, payload, ESC/\ terminator) between reads.
        let stream = b"\x1b]0;title\x07\x1b]133;A\x1b\\plain\x1b]7;file://h/x\x07";
        let chunks: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(
            collect(&[b"7", b"133"], &chunks),
            vec![b"133;A".to_vec(), b"7;file://h/x".to_vec()]
        );
    }

    #[test]
    fn ignored_sequence_split_across_reads_still_recovers() {
        // An uninteresting OSC torn across chunks must keep being discarded
        // (state persists across `feed`s), and the next interesting one lands.
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
        // ESC ESC ] still opens an OSC (the last ESC wins).
        assert_eq!(
            collect(&[b"9"], &[b"\x1b\x1b]9;ok\x07"]),
            vec![b"9;ok".to_vec()]
        );
        // An ESC inside an OSC followed by a non-OSC escape abandons cleanly.
        assert_eq!(
            collect(&[b"9"], &[b"\x1b]9;half\x1b[0m\x1b]9;whole\x07"]),
            vec![b"9;whole".to_vec()]
        );
    }
}

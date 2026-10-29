//! The ED2 parity rewriter (Phase 4): `CSI 2 J` on the primary screen
//! becomes `CSI 22 J`, so a clear without shell integration keeps the
//! screen in history the way the replaced engine always did.
//!
//! Ghostty 1.4 already scrolls on ED2 when the last non-empty row carries
//! an OSC 133 prompt mark (Terminal.zig's at-prompt heuristic, #905), and
//! `CSI 22 J` is its explicit scroll-clear. What's left is the tail the
//! heuristic can't see — every remote shell without tty59's integration
//! (an SSH session is exactly that), where `^L` and `clear` would
//! otherwise drop history — so the rewrite covers it uniformly.
//!
//! The rewriter is a **parser-state scanner**, not a substring match:
//!
//! - it tracks just enough VT state to know when bytes are at *ground*:
//!   an `ESC [` inside an OSC/DCS/APC/PM/SOS payload — a title carrying
//!   the bytes `ESC[2J`, a kitty APC frame — is payload and passes
//!   through untouched, across chunk boundaries too (an unterminated
//!   string at a chunk's end carries its state to the next);
//! - a candidate `CSI … J` is buffered from its `ESC [` to its final
//!   byte, so a read boundary inside the sequence cannot fool it: the
//!   buffered bytes (rewritten or verbatim) are flushed ahead of the
//!   bytes that follow them;
//! - it rewrites exactly ground-state `CSI 2 J` — parameter span `2`, no
//!   private marker, no intermediates; `CSI 1;2 J`, `CSI ? 2 J` and
//!   friends pass through — and only while the primary screen is
//!   showing, a fact it tracks *within* the chunk too, so a buffer that
//!   leaves the alternate screen (`CSI ? 1049 l`) and clears in the same
//!   read still rewrites. The alternate screen has no scrollback, and
//!   neither engine pushes there.
//!
//! The plain-text hot path stays plain: chunks are scanned with `memchr`
//! for ESC, so a chunk with no escape costs one pass of vector compares
//! and zero copies.

/// The rewriter's carry state across `advance` calls: the tail of a
/// candidate sequence a read boundary split, and whether the stream is
/// currently inside a string payload.
#[derive(Default)]
pub(crate) struct Ed2Rewriter {
    /// Bytes of a candidate `CSI … J` seen so far, from its `ESC` on.
    /// Non-empty only at ground.
    pending: Vec<u8>,
    /// An OSC/DCS/APC/PM/SOS payload is open (its introducer was consumed,
    /// its terminator not yet). Bytes while this is set are payload.
    in_string: bool,
}

impl Ed2Rewriter {
    /// Rewrite one chunk. Returns the bytes to feed the engine: `input`
    /// verbatim when nothing needed changing, or a rebuilt buffer with
    /// ground-state `CSI 2 J` spliced into `CSI 22 J`.
    pub(crate) fn rewrite<'a>(
        &mut self,
        input: &'a [u8],
        out: &'a mut Vec<u8>,
        primary_at_start: bool,
    ) -> &'a [u8] {
        // The common case first: ground, no open candidate, and no ESC in
        // the chunk — the plaintext hot path. One memchr, no copy.
        if self.pending.is_empty() && !self.in_string {
            if memchr::memchr(0x1b, input).is_none() {
                return input;
            }
            out.clear();
            out.extend_from_slice(input);
            self.scan(out, primary_at_start);
            return out;
        }
        // A string payload or a split candidate was open: stage the carry
        // ahead of this chunk and scan the whole staging buffer.
        out.clear();
        out.extend_from_slice(&self.pending);
        out.extend_from_slice(input);
        self.pending.clear();
        let was_string = self.in_string;
        self.scan_carried(out, was_string, primary_at_start);
        out
    }

    /// Scan a buffer that starts at ground.
    fn scan(&mut self, buf: &mut Vec<u8>, primary: bool) {
        self.scan_carried(buf, false, primary);
    }

    /// Scan `buf`, which starts inside a string payload iff `was_string`.
    /// Bytes the scanner could not yet classify (a split candidate, or a
    /// split string) move back into the carry and the buffer is truncated
    /// to what can be fed now.
    fn scan_carried(&mut self, buf: &mut Vec<u8>, was_string: bool, mut primary: bool) {
        let mut i = 0usize;
        if was_string {
            let Some(end) = string_end(&buf[i..]) else {
                // The payload runs past this chunk too: all of it passes.
                self.in_string = true;
                return;
            };
            self.in_string = false;
            i += end;
        }
        while i < buf.len() {
            // From ground, the next ESC is the only byte that matters.
            let Some(rel) = memchr::memchr(0x1b, &buf[i..]) else {
                return;
            };
            let esc = i + rel;
            let Some(&after) = buf.get(esc + 1) else {
                // A trailing ESC: it may open a CSI the next chunk
                // completes. Carry it.
                self.pending.extend_from_slice(&buf[esc..]);
                buf.truncate(esc);
                return;
            };
            match after {
                b'[' => {}
                b']' | b'P' | b'_' | b'^' | b'X' => {
                    // A string payload opens. Its bytes pass through
                    // verbatim until BEL or ST — including anything that
                    // looks like a CSI inside it.
                    match string_end(&buf[esc + 2..]) {
                        Some(end) => {
                            self.in_string = false;
                            i = esc + 2 + end;
                        }
                        None => {
                            // Unterminated at the chunk's end: everything
                            // from the introducer on is payload; the state
                            // carries.
                            self.in_string = true;
                            return;
                        }
                    }
                    continue;
                }
                0x1b => {
                    // ESC ESC: the first aborts, the second starts fresh —
                    // rescan from the second.
                    i = esc + 1;
                    continue;
                }
                _ => {
                    // A two-byte escape (ESC 7, ESC 8, ESC M, charset
                    // selection…): back to ground right after it.
                    i = esc + 2;
                    continue;
                }
            }
            // A CSI: parameter and intermediate bytes (0x20..=0x3f) up to
            // a final byte (0x40..=0x7e). A control byte inside is
            // malformed (CAN/SUB territory) — the engine's parser sorts
            // it out; the scanner resumes at ground after it.
            let mut j = esc + 2;
            let params_start = j;
            let well_formed = loop {
                let Some(&b) = buf.get(j) else {
                    // Split mid-sequence: carry the candidate whole.
                    self.pending.extend_from_slice(&buf[esc..]);
                    buf.truncate(esc);
                    return;
                };
                if (0x40..=0x7e).contains(&b) {
                    break true;
                }
                if !(0x20..=0x3f).contains(&b) {
                    break false;
                }
                j += 1;
            };
            if !well_formed {
                i = j + 1;
                continue;
            }
            let params = &buf[params_start..j];
            match buf[j] {
                // The screen switches: track the primary-screen fact for
                // the rest of this buffer (1048 saves the cursor only).
                b'h' | b'l' if params == b"?1049" || params == b"?47" || params == b"?1047" => {
                    primary = buf[j] == b'l';
                }
                // The rewrite itself: exactly `CSI 2 J`, ground state,
                // primary screen.
                b'J' if primary && params == b"2" => {
                    // `2` → `22`: one byte becomes two, spliced in place.
                    buf[params_start] = b'2';
                    buf.insert(params_start + 1, b'2');
                    j += 1;
                }
                _ => {}
            }
            i = j + 1;
        }
    }
}

/// Where a string payload ends within `buf`: just past its BEL or ST
/// (`ESC \`), else `None` (unterminated). An ESC inside a payload that is
/// not the ST's ESC is payload itself.
fn string_end(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            0x07 => return Some(i + 1),
            0x1b => {
                if buf.get(i + 1) == Some(&b'\\') {
                    return Some(i + 2);
                }
                // A lone ESC before a non-`\`: payload (a cancelled ST
                // attempt is the engine parser's problem, not ground's).
                i += 2;
            }
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the rewriter the way `advance` does, at the given chunk
    /// splits, and return what the engine would be fed.
    fn fed(input: &[u8], splits: &[usize], primary: bool) -> Vec<u8> {
        let mut rewriter = Ed2Rewriter::default();
        let mut scratch = Vec::new();
        let mut all = Vec::new();
        let mut at = 0usize;
        for &split in splits.iter().chain(std::iter::once(&input.len())) {
            let chunk = rewriter.rewrite(&input[at..split], &mut scratch, primary);
            all.extend_from_slice(chunk);
            at = split;
        }
        all
    }

    #[test]
    fn a_plain_ed2_is_rewritten_on_the_primary_screen() {
        assert_eq!(fed(b"\x1b[H\x1b[2J", &[], true), b"\x1b[H\x1b[22J");
        // …and not on the alternate screen.
        assert_eq!(fed(b"\x1b[H\x1b[2J", &[], false), b"\x1b[H\x1b[2J");
    }

    #[test]
    fn a_split_ed2_is_rewritten_wherever_the_cut_lands() {
        let input = b"out\x1b[2J";
        for cut in 0..=input.len() {
            assert_eq!(
                fed(input, &[cut], true),
                b"out\x1b[22J",
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn an_ed2_inside_an_osc_title_passes_through() {
        let input = b"\x1b]0;bad \x1b[2J title\x07after\x1b[2J";
        assert_eq!(
            fed(input, &[], true),
            b"\x1b]0;bad \x1b[2J title\x07after\x1b[22J",
            "the payload's bytes are not a sequence; the one after the title is"
        );
        // …and the split lands inside the unterminated title.
        for cut in 0..=input.len() {
            assert_eq!(
                fed(input, &[cut], true),
                b"\x1b]0;bad \x1b[2J title\x07after\x1b[22J",
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn an_ed2_inside_an_apc_frame_passes_through() {
        let input = b"\x1b_Ga=T,f=24,s=1,v=1;\x1b[2J\x1b\\tail\x1b[2J";
        assert_eq!(
            fed(input, &[], true),
            b"\x1b_Ga=T,f=24,s=1,v=1;\x1b[2J\x1b\\tail\x1b[22J"
        );
    }

    #[test]
    fn near_misses_are_untouched() {
        for seq in [&b"\x1b[1;2J"[..], b"\x1b[?2J", b"\x1b[22J", b"\x1b[0J", b"\x1b[J"] {
            assert_eq!(fed(seq, &[], true), seq, "{seq:?} is not a plain ED2");
        }
    }

    #[test]
    fn a_screen_switch_and_a_clear_in_one_chunk_track_the_screen() {
        // Leaving the alternate screen and clearing in the same read: the
        // clear lands on the primary screen, so it rewrites.
        assert_eq!(fed(b"\x1b[?1049l\x1b[2J", &[], false), b"\x1b[?1049l\x1b[22J");
        // Entering the alternate screen then clearing: no rewrite.
        assert_eq!(fed(b"\x1b[?1049h\x1b[2J", &[], true), b"\x1b[?1049h\x1b[2J");
    }

    #[test]
    fn plaintext_with_no_escape_passes_at_vector_speed() {
        assert_eq!(fed(b"just text\r\nand more\r\n", &[], true), b"just text\r\nand more\r\n");
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// The rewriter's share of the reader hot path, over the plaintext
    /// workload's shape: no ESC at all, and ESC-dense text.
    #[test]
    #[ignore = "latency measurement, not an assertion (see module docs)"]
    fn bench_rewriter_plaintext() {
        let words = "to be or not to be that is the question ";
        let mut bytes = Vec::new();
        while bytes.len() < 4 << 20 {
            bytes.extend_from_slice(words.as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }
        let mut rewriter = Ed2Rewriter::default();
        let mut out = Vec::new();
        // Warm.
        for chunk in bytes.chunks(64 * 1024) {
            rewriter.rewrite(chunk, &mut out, true);
        }
        let t = std::time::Instant::now();
        for chunk in bytes.chunks(64 * 1024) {
            std::hint::black_box(rewriter.rewrite(chunk, &mut out, true));
        }
        let plain = t.elapsed();

        // ESC-dense: SGR churn every few words (the worst realistic mix).
        let mut dense = Vec::new();
        while dense.len() < 4 << 20 {
            dense.extend_from_slice(b"\x1b[1;31m");
            dense.extend_from_slice(words.as_bytes());
            dense.extend_from_slice(b"\x1b[0m");
        }
        let mut rewriter = Ed2Rewriter::default();
        for chunk in dense.chunks(64 * 1024) {
            rewriter.rewrite(chunk, &mut out, true);
        }
        let t = std::time::Instant::now();
        for chunk in dense.chunks(64 * 1024) {
            std::hint::black_box(rewriter.rewrite(chunk, &mut out, true));
        }
        let esc = t.elapsed();

        let mb = |n: usize, d: std::time::Duration| n as f64 / 1e6 / d.as_secs_f64();
        eprintln!(
            "ed2 rewriter: plaintext {:.0} MB/s ({:.1} ms/11.5MB), esc-dense {:.0} MB/s ({:.1} ms/11.5MB)",
            mb(bytes.len(), plain),
            plain.as_secs_f64() * 1000.0 * (11.5 / 4.0),
            mb(dense.len(), esc),
            esc.as_secs_f64() * 1000.0 * (11.5 / 4.0),
        );
    }
}

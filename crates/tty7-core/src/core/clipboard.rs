//! Streaming OSC 5522 clipboard-write support.
//!
//! Clipboard packets are intercepted in the daemon before they reach the replay
//! ring. Completed writes travel to the GUI as compact binary frames; only the
//! GUI is allowed to touch the system clipboard.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;

pub const MAX_CLIPBOARD_BYTES: usize = 16 << 20;
const MAX_CHUNK_BYTES: usize = 4096;
const MAX_OSC_PAYLOAD: usize = 8 << 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardWrite {
    pub mime: String,
    pub data: Vec<u8>,
    pub id: Option<String>,
}

impl ClipboardWrite {
    pub fn encode_frame(self) -> Vec<u8> {
        let mime = self.mime.as_bytes();
        let id = self.id.as_deref().unwrap_or_default().as_bytes();
        let mut out = Vec::with_capacity(4 + mime.len() + id.len() + self.data.len());
        out.extend_from_slice(&(mime.len() as u16).to_le_bytes());
        out.extend_from_slice(&(id.len() as u16).to_le_bytes());
        out.extend_from_slice(mime);
        out.extend_from_slice(id);
        out.extend_from_slice(&self.data);
        out
    }

    pub fn decode_frame(frame: Vec<u8>) -> Option<Self> {
        let mime_len = usize::from(u16::from_le_bytes(frame.get(..2)?.try_into().ok()?));
        let id_len = usize::from(u16::from_le_bytes(frame.get(2..4)?.try_into().ok()?));
        let data_at = 4usize.checked_add(mime_len)?.checked_add(id_len)?;
        if data_at > frame.len() || frame.len() - data_at > MAX_CLIPBOARD_BYTES {
            return None;
        }
        let mime = std::str::from_utf8(frame.get(4..4 + mime_len)?)
            .ok()?
            .to_string();
        let id = match id_len {
            0 => None,
            _ => Some(
                std::str::from_utf8(frame.get(4 + mime_len..data_at)?)
                    .ok()?
                    .to_string(),
            ),
        };
        Some(Self {
            mime,
            data: frame[data_at..].to_vec(),
            id,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Write(ClipboardWrite),
    Reply(Vec<u8>),
}

#[derive(Default)]
struct WriteState {
    id: Option<String>,
    mime: Option<String>,
    data: Vec<u8>,
    failed: bool,
}

#[derive(Default)]
struct Parser {
    write: Option<WriteState>,
    allowed: bool,
    available: bool,
}

impl Parser {
    fn feed(&mut self, payload: &[u8]) -> Option<Event> {
        if payload == b"probe" {
            return Some(Event::Reply(capability_reply(self.allowed)));
        }
        if payload == b"invalid" {
            return self.fail("EINVAL");
        }
        let (metadata, data) = match payload.iter().position(|&b| b == b';') {
            Some(at) => (&payload[..at], &payload[at + 1..]),
            None => (payload, &[][..]),
        };
        let metadata = std::str::from_utf8(metadata).ok()?;
        let fields = metadata
            .split(':')
            .filter_map(|field| field.split_once('='))
            .collect::<Vec<_>>();
        let kind = fields.iter().find(|(k, _)| *k == "type")?.1;
        let id = fields
            .iter()
            .find(|(k, _)| *k == "id")
            .map(|(_, value)| sanitize_id(value))
            .filter(|value| !value.is_empty());

        match kind {
            "read" => Some(Event::Reply(response_for("read", id.as_deref(), "ENOSYS"))),
            "write" => {
                if !self.allowed {
                    self.write = Some(WriteState {
                        id: id.clone(),
                        failed: true,
                        ..WriteState::default()
                    });
                    return Some(Event::Reply(response(id.as_deref(), "EPERM")));
                }
                if !self.available {
                    self.write = Some(WriteState {
                        id: id.clone(),
                        failed: true,
                        ..WriteState::default()
                    });
                    return Some(Event::Reply(response(id.as_deref(), "EBUSY")));
                }
                if fields
                    .iter()
                    .any(|(k, value)| *k == "loc" && *value == "primary")
                {
                    self.write = Some(WriteState {
                        id: id.clone(),
                        failed: true,
                        ..WriteState::default()
                    });
                    return Some(Event::Reply(response(id.as_deref(), "ENOSYS")));
                }
                self.write = Some(WriteState {
                    id,
                    ..WriteState::default()
                });
                None
            }
            "wdata" => self.feed_data(fields, data),
            _ => None,
        }
    }

    fn feed_data(&mut self, fields: Vec<(&str, &str)>, payload: &[u8]) -> Option<Event> {
        if self.write.as_ref()?.failed {
            return None;
        }
        if !self.allowed {
            return self.fail("EPERM");
        }
        let state = self.write.as_mut()?;
        let mime = fields
            .iter()
            .find(|(k, _)| *k == "mime")
            .map(|(_, value)| *value);

        if mime.is_none() && payload.is_empty() {
            let state = self.write.take()?;
            let Some(mime) = state.mime else {
                return Some(Event::Reply(response(state.id.as_deref(), "EINVAL")));
            };
            if state.data.is_empty() {
                return Some(Event::Reply(response(state.id.as_deref(), "EINVAL")));
            }
            return Some(Event::Write(ClipboardWrite {
                mime,
                data: state.data,
                id: state.id,
            }));
        }

        let Some(encoded_mime) = mime else {
            return self.fail("EINVAL");
        };
        let Ok(mime_bytes) = BASE64.decode(encoded_mime) else {
            return self.fail("EINVAL");
        };
        let Ok(mime) = std::str::from_utf8(&mime_bytes) else {
            return self.fail("EINVAL");
        };
        if !supported_mime(mime) || state.mime.as_deref().is_some_and(|current| current != mime) {
            return self.fail("EINVAL");
        }
        let Ok(chunk) = BASE64.decode(payload) else {
            return self.fail("EINVAL");
        };
        if chunk.is_empty()
            || chunk.len() > MAX_CHUNK_BYTES
            || state.data.len().saturating_add(chunk.len()) > MAX_CLIPBOARD_BYTES
        {
            return self.fail("EINVAL");
        }
        state.mime.get_or_insert_with(|| mime.to_string());
        state.data.extend_from_slice(&chunk);
        None
    }

    fn fail(&mut self, status: &str) -> Option<Event> {
        let state = self.write.as_mut()?;
        state.failed = true;
        // Nothing reads a failed transfer's bytes again, and the state itself
        // lives on until the next `type=write` or a change of controller. A
        // sender that fails its last chunk on purpose would otherwise leave
        // `MAX_CLIPBOARD_BYTES` of its own image parked in every pane it can
        // reach, for as long as it likes.
        state.data = Vec::new();
        Some(Event::Reply(response(state.id.as_deref(), status)))
    }
}

/// The DECRPM answer to `CSI ? 5522 $ p`: 1 for a mode that is set, 2 for one
/// this terminal knows but has switched off. A terminal that never heard of
/// OSC 5522 answers 0, so a sender can still tell "not supported" from "not
/// permitted here" — which it cannot do if the answer never moves.
fn capability_reply(allowed: bool) -> Vec<u8> {
    match allowed {
        true => b"\x1b[?5522;1$y".to_vec(),
        false => b"\x1b[?5522;2$y".to_vec(),
    }
}

fn supported_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/jpg" | "image/gif" | "image/webp"
    )
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '.'))
        .collect()
}

pub fn response(id: Option<&str>, status: &str) -> Vec<u8> {
    response_for("write", id, status)
}

fn response_for(kind: &str, id: Option<&str>, status: &str) -> Vec<u8> {
    let id = id
        .map(sanitize_id)
        .filter(|value| !value.is_empty())
        .map(|value| format!(":id={value}"))
        .unwrap_or_default();
    format!("\x1b]5522;type={kind}:status={status}{id}\x1b\\").into_bytes()
}

#[derive(Default)]
pub struct ClipboardSniffer {
    tokenizer: Tokenizer,
    parser: Parser,
    controller_epoch: Option<u64>,
}

impl ClipboardSniffer {
    pub fn set_controller(&mut self, epoch: Option<u64>, allowed: bool) {
        if self.controller_epoch != epoch {
            self.parser.write = None;
            self.controller_epoch = epoch;
        }
        self.parser.allowed = allowed;
        self.parser.available = epoch.is_some();
    }

    pub fn sniff<'a>(&mut self, bytes: &'a [u8]) -> Sniffed<'a> {
        if self.tokenizer.ground() && !might_contain_protocol(bytes) {
            return Sniffed::Plain(bytes);
        }
        let Self {
            tokenizer, parser, ..
        } = self;
        let segments = std::cell::RefCell::new(Vec::new());
        tokenizer.feed(
            bytes,
            |run| push_output(&mut segments.borrow_mut(), run),
            |payload| {
                if let Some(event) = parser.feed(payload) {
                    segments.borrow_mut().push(Segment::Event(event));
                }
            },
        );
        Sniffed::Segments(segments.into_inner())
    }
}

fn might_contain_protocol(bytes: &[u8]) -> bool {
    const PREFIXES: [&[u8]; 2] = [b"\x1b]5522;", b"\x1b[?5522$p"];
    PREFIXES.iter().any(|prefix| {
        memchr::memmem::find(bytes, prefix).is_some()
            || (1..prefix.len())
                .any(|len| bytes.len() >= len && bytes[bytes.len() - len..] == prefix[..len])
    })
}

fn push_output(segments: &mut Vec<Segment>, run: &[u8]) {
    if run.is_empty() {
        return;
    }
    if let Some(Segment::Output(out)) = segments.last_mut() {
        out.extend_from_slice(run);
    } else {
        segments.push(Segment::Output(run.to_vec()));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Output(Vec<u8>),
    Event(Event),
}

pub enum Sniffed<'a> {
    Plain(&'a [u8]),
    Segments(Vec<Segment>),
}

#[derive(Default)]
struct Tokenizer {
    state: TokenState,
    buf: Vec<u8>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum TokenState {
    #[default]
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    PassOsc,
    PassOscEsc,
    Drop,
    DropEsc,
}

impl Tokenizer {
    fn ground(&self) -> bool {
        self.state == TokenState::Ground
    }

    fn feed(
        &mut self,
        bytes: &[u8],
        mut on_output: impl FnMut(&[u8]),
        mut on_payload: impl FnMut(&[u8]),
    ) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                TokenState::Ground => match memchr::memchr(0x1b, &bytes[i..]) {
                    Some(off) => {
                        on_output(&bytes[i..i + off]);
                        self.state = TokenState::Esc;
                        i += off + 1;
                    }
                    None => {
                        on_output(&bytes[i..]);
                        return;
                    }
                },
                TokenState::Esc => {
                    if bytes[i] == b']' {
                        self.buf.clear();
                        self.state = TokenState::Osc;
                        i += 1;
                    } else if bytes[i] == b'[' {
                        self.buf.clear();
                        self.state = TokenState::Csi;
                        i += 1;
                    } else {
                        on_output(b"\x1b");
                        self.state = TokenState::Ground;
                    }
                }
                TokenState::Csi => {
                    const QUERY: &[u8] = b"?5522$p";
                    self.buf.push(bytes[i]);
                    i += 1;
                    if self.buf.as_slice() == QUERY {
                        on_payload(b"probe");
                        self.buf.clear();
                        self.state = TokenState::Ground;
                    } else if !QUERY.starts_with(&self.buf) {
                        on_output(b"\x1b[");
                        on_output(&self.buf);
                        self.buf.clear();
                        self.state = TokenState::Ground;
                    }
                }
                TokenState::Osc => match bytes[i] {
                    0x07 => {
                        self.finish(false, &mut on_output, &mut on_payload);
                        i += 1;
                    }
                    0x1b => {
                        self.state = TokenState::OscEsc;
                        i += 1;
                    }
                    b => {
                        self.buf.push(b);
                        i += 1;
                        let target = b"5522";
                        let wrong_id = match self.buf.iter().position(|&b| b == b';') {
                            Some(pos) => &self.buf[..pos] != target,
                            None => !target.starts_with(&self.buf),
                        };
                        if wrong_id {
                            on_output(b"\x1b]");
                            on_output(&self.buf);
                            self.buf.clear();
                            self.state = TokenState::PassOsc;
                        } else if self.buf.len() > MAX_OSC_PAYLOAD {
                            self.buf.clear();
                            self.state = TokenState::Drop;
                        }
                    }
                },
                TokenState::OscEsc => {
                    if bytes[i] == b'\\' {
                        self.finish(true, &mut on_output, &mut on_payload);
                        i += 1;
                    } else if bytes[i] == b']' {
                        self.buf.clear();
                        self.state = TokenState::Osc;
                        i += 1;
                    } else {
                        self.buf.clear();
                        self.state = TokenState::Ground;
                    }
                }
                TokenState::PassOsc => match memchr::memchr2(0x07, 0x1b, &bytes[i..]) {
                    Some(off) => {
                        on_output(&bytes[i..i + off + 1]);
                        self.state = if bytes[i + off] == 0x07 {
                            TokenState::Ground
                        } else {
                            TokenState::PassOscEsc
                        };
                        i += off + 1;
                    }
                    None => {
                        on_output(&bytes[i..]);
                        return;
                    }
                },
                TokenState::PassOscEsc => {
                    on_output(&bytes[i..i + 1]);
                    self.state = if bytes[i] == b'\\' {
                        TokenState::Ground
                    } else {
                        TokenState::PassOsc
                    };
                    i += 1;
                }
                TokenState::Drop => match memchr::memchr2(0x07, 0x1b, &bytes[i..]) {
                    Some(off) => {
                        if bytes[i + off] == 0x07 {
                            on_payload(b"invalid");
                            self.state = TokenState::Ground;
                        } else {
                            self.state = TokenState::DropEsc;
                        }
                        i += off + 1;
                    }
                    None => return,
                },
                TokenState::DropEsc => {
                    if bytes[i] == b'\\' {
                        on_payload(b"invalid");
                        self.state = TokenState::Ground;
                    } else if bytes[i] == b']' {
                        self.buf.clear();
                        self.state = TokenState::Osc;
                    } else {
                        self.state = TokenState::Drop;
                    }
                    i += 1;
                }
            }
        }
    }

    fn finish(
        &mut self,
        st: bool,
        on_output: &mut impl FnMut(&[u8]),
        on_payload: &mut impl FnMut(&[u8]),
    ) {
        if let Some(payload) = self.buf.strip_prefix(b"5522;") {
            on_payload(payload);
        } else {
            on_output(b"\x1b]");
            on_output(&self.buf);
            on_output(if st { b"\x1b\\" } else { b"\x07" });
        }
        self.buf.clear();
        self.state = TokenState::Ground;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(metadata: &str, payload: &str) -> Vec<u8> {
        let separator = if payload.is_empty() { "" } else { ";" };
        format!("\x1b]5522;{metadata}{separator}{payload}\x1b\\").into_bytes()
    }

    fn collect(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Event>) {
        let mut sniffer = ClipboardSniffer::default();
        sniffer.set_controller(Some(1), true);
        let mut output = Vec::new();
        let mut events = Vec::new();
        for chunk in chunks {
            match sniffer.sniff(chunk) {
                Sniffed::Plain(bytes) => output.extend_from_slice(bytes),
                Sniffed::Segments(parts) => {
                    for part in parts {
                        match part {
                            Segment::Output(bytes) => output.extend(bytes),
                            Segment::Event(event) => events.push(event),
                        }
                    }
                }
            }
        }
        (output, events)
    }

    #[test]
    fn image_write_is_reassembled_and_stripped() {
        let mime = BASE64.encode("image/png");
        let mut stream = b"before".to_vec();
        stream.extend(osc("type=write:id=req.1", ""));
        stream.extend(osc(
            &format!("type=wdata:mime={mime}"),
            &BASE64.encode(b"png"),
        ));
        stream.extend(osc(
            &format!("type=wdata:mime={mime}"),
            &BASE64.encode(b"-data"),
        ));
        stream.extend(osc("type=wdata", ""));
        stream.extend_from_slice(b"after");

        let split = stream.len() / 2;
        let (output, events) = collect(&[&stream[..split], &stream[split..]]);
        assert_eq!(output, b"beforeafter");
        assert_eq!(
            events,
            vec![Event::Write(ClipboardWrite {
                mime: "image/png".into(),
                data: b"png-data".to_vec(),
                id: Some("req.1".into()),
            })]
        );
    }

    #[test]
    fn non_clipboard_osc_passes_through() {
        let input = b"a\x1b]0;title\x07b\x1b]133;A\x1b\\c";
        let (output, events) = collect(&[input]);
        assert_eq!(output, input);
        assert!(events.is_empty());
    }

    #[test]
    fn invalid_or_unsupported_data_is_rejected_once() {
        let mime = BASE64.encode("image/svg+xml");
        let (output, events) = collect(&[
            &osc("type=write:id=bad/one", ""),
            &osc(
                &format!("type=wdata:mime={mime}"),
                &BASE64.encode(b"<svg/>"),
            ),
            &osc(
                &format!("type=wdata:mime={mime}"),
                &BASE64.encode(b"ignored"),
            ),
            &osc("type=wdata", ""),
        ]);
        assert!(output.is_empty());
        assert_eq!(
            events,
            vec![Event::Reply(response(Some("badone"), "EINVAL"))]
        );
    }

    #[test]
    fn chunks_and_total_size_are_bounded() {
        let mime = BASE64.encode("image/png");
        let too_big = vec![0u8; MAX_CHUNK_BYTES + 1];
        let (_, events) = collect(&[
            &osc("type=write", ""),
            &osc(&format!("type=wdata:mime={mime}"), &BASE64.encode(too_big)),
        ]);
        assert_eq!(events, vec![Event::Reply(response(None, "EINVAL"))]);

        let mut parser = Parser {
            allowed: true,
            write: Some(WriteState {
                mime: Some("image/png".into()),
                data: vec![0; MAX_CLIPBOARD_BYTES],
                ..WriteState::default()
            }),
            ..Parser::default()
        };
        assert_eq!(
            parser.feed_data(
                vec![("type", "wdata"), ("mime", mime.as_str())],
                BASE64.encode(b"x").as_bytes()
            ),
            Some(Event::Reply(response(None, "EINVAL")))
        );
    }

    #[test]
    fn primary_selection_is_not_supported() {
        let (_, events) = collect(&[&osc("type=write:loc=primary:id=x", "")]);
        assert_eq!(events, vec![Event::Reply(response(Some("x"), "ENOSYS"))]);
    }

    #[test]
    fn disabled_writes_fail_before_any_image_data_is_buffered() {
        let mut sniffer = ClipboardSniffer::default();
        let start = osc("type=write:id=denied", "");
        let events = match sniffer.sniff(&start) {
            Sniffed::Segments(parts) => parts,
            Sniffed::Plain(_) => panic!("OSC 5522 must be intercepted"),
        };
        assert_eq!(
            events,
            vec![Segment::Event(Event::Reply(response(
                Some("denied"),
                "EPERM"
            )))]
        );
        assert!(sniffer.parser.write.as_ref().unwrap().data.is_empty());
    }

    #[test]
    fn allowed_write_without_a_controller_is_busy() {
        let mut sniffer = ClipboardSniffer::default();
        sniffer.set_controller(None, true);
        let events = match sniffer.sniff(&osc("type=write:id=early", "")) {
            Sniffed::Segments(parts) => parts,
            Sniffed::Plain(_) => panic!("OSC 5522 must be intercepted"),
        };
        assert_eq!(
            events,
            vec![Segment::Event(Event::Reply(response(
                Some("early"),
                "EBUSY"
            )))]
        );
    }

    #[test]
    fn byte_at_a_time_delivery_preserves_text_and_reassembles_the_write() {
        let mime = BASE64.encode("image/png");
        let data = BASE64.encode(b"png");
        let stream = format!(
            "a\x1b]5522;type=write\x1b\\\
             \x1b]5522;type=wdata:mime={mime};{data}\x1b\\\
             \x1b]5522;type=wdata\x1b\\b"
        );
        let chunks: Vec<&[u8]> = stream.as_bytes().chunks(1).collect();
        let (output, events) = collect(&chunks);
        assert_eq!(output, b"ab");
        assert!(matches!(
            events.as_slice(),
            [Event::Write(ClipboardWrite { mime, data, .. })]
                if mime == "image/png" && data == b"png"
        ));
    }

    #[test]
    fn capability_probe_is_stripped_and_answered_across_reads() {
        let (output, events) = collect(&[b"a\x1b[?55", b"22$p", b"b"]);
        assert_eq!(output, b"ab");
        assert_eq!(events, vec![Event::Reply(capability_reply(true))]);
    }

    /// The probe has to answer the permission actually in force. A sender that
    /// reads DECRPM sees 2 — "this terminal knows the mode and it is off" — for
    /// a host the user never opted in, and stops there instead of pushing an
    /// image nobody will take.
    #[test]
    fn the_probe_reports_a_host_that_is_not_permitted_as_off() {
        let mut sniffer = ClipboardSniffer::default();
        sniffer.set_controller(Some(1), false);
        let events = match sniffer.sniff(b"\x1b[?5522$p") {
            Sniffed::Segments(parts) => parts,
            Sniffed::Plain(_) => panic!("the probe must be intercepted"),
        };
        assert_eq!(
            events,
            vec![Segment::Event(Event::Reply(capability_reply(false)))]
        );
        assert_ne!(capability_reply(true), capability_reply(false));
    }

    /// A transfer that fails half way stops being somewhere to park an image:
    /// the reply goes out and the bytes go with it.
    #[test]
    fn a_failed_transfer_releases_what_it_had_buffered() {
        let png = BASE64.encode("image/png");
        let gif = BASE64.encode("image/gif");
        let mut sniffer = ClipboardSniffer::default();
        sniffer.set_controller(Some(1), true);
        let _ = sniffer.sniff(&osc("type=write:id=half", ""));
        let _ = sniffer.sniff(&osc(
            &format!("type=wdata:mime={png}"),
            &BASE64.encode(b"png-bytes"),
        ));
        assert!(!sniffer.parser.write.as_ref().unwrap().data.is_empty());

        let events = match sniffer.sniff(&osc(
            &format!("type=wdata:mime={gif}"),
            &BASE64.encode(b"gif"),
        )) {
            Sniffed::Segments(parts) => parts,
            Sniffed::Plain(_) => panic!("OSC 5522 must be intercepted"),
        };
        assert_eq!(
            events,
            vec![Segment::Event(Event::Reply(response(
                Some("half"),
                "EINVAL"
            )))]
        );
        assert!(sniffer.parser.write.as_ref().unwrap().data.is_empty());
    }

    #[test]
    fn clipboard_reads_are_explicitly_unsupported() {
        let (_, events) = collect(&[&osc("type=read:id=read-1", &BASE64.encode("image/png"))]);
        assert_eq!(
            events,
            vec![Event::Reply(response_for("read", Some("read-1"), "ENOSYS"))]
        );
    }

    #[test]
    fn changing_controller_discards_an_unfinished_write() {
        let mime = BASE64.encode("image/png");
        let mut sniffer = ClipboardSniffer::default();
        sniffer.set_controller(Some(1), true);
        let _ = sniffer.sniff(&osc("type=write:id=old", ""));
        let _ = sniffer.sniff(&osc(
            &format!("type=wdata:mime={mime}"),
            &BASE64.encode(b"old"),
        ));

        sniffer.set_controller(Some(2), true);
        let events = match sniffer.sniff(&osc("type=wdata", "")) {
            Sniffed::Segments(parts) => parts,
            Sniffed::Plain(_) => panic!("OSC 5522 must be intercepted"),
        };
        assert!(
            events.is_empty(),
            "a new controller must not receive the old controller's partial write"
        );
    }
}

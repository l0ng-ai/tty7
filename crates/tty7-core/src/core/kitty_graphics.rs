//! Streaming kitty-graphics-protocol extractor (APC `ESC _ G … ST`).
//!
//! The daemon-side counterpart to [`crate::core::osc`]: a tiny, resumable state
//! machine that pulls kitty graphics commands out of the raw PTY byte stream
//! *without* running a full VT parser. It exists for the same reason the OSC
//! sniffer does — the client's `alacritty_terminal` fork silently discards APC
//! sequences (`ESC _` routes to vte's `SosPmApcString`, whose payload bytes hit
//! a `_ => ()` arm), so an image transmitted this way never surfaces as a `Term`
//! event. We tap the bytes here instead.
//!
//! This lives daemon-side so the pixel payload can be lifted out of the stream
//! *before* it enters the replay ring — a full-window RGBA frame is hundreds of
//! KB, and letting it accumulate in the ring would make reattach replay
//! catastrophic. The daemon decodes here and forwards a compact out-of-band
//! frame to the client; the base64 text never rides the socket, and the VT
//! parser never has to chew through it.
//!
//! Scope: this handles the subset the wire actually carries in practice
//! (transmit-and-display, query, delete). Which transmission media we accept
//! depends on where the pane lives:
//!
//! - Direct (`t=d`, base64 inline) is always honored — it is the only medium
//!   that survives tty7's socket + SSH topology.
//! - File (`t=f`/`t=t`) and shared memory (`t=s`) are honored only on a *local*
//!   unix pane, where the name resolves on this host and the daemon can read it
//!   without anything crossing a tunnel. Everywhere else — a remote pane, or a
//!   non-unix host where [`MediumTransfer::resolve`] can't do the work — the
//!   probe is answered *unsupported* so a sender like `terminal-browser` falls
//!   back to `t=d` on its own rather than transmitting into a black hole.
//!
//! Protocol reference: <https://sw.kovidgoyal.net/kitty/graphics-protocol/>

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

/// Cap on a single APC payload (one chunk) we'll buffer before abandoning it.
/// `terminal-browser` chunks direct transmissions into 4 KiB base64 pieces, but
/// the spec only *recommends* chunking — a sender is free to put a whole frame
/// in one command, and an uncompressed 4K RGBA frame is ~44 MB of base64. Size
/// this to admit a one-shot frame that still fits [`MAX_TRANSMISSION_BASE64`];
/// [`ApcTokenizer::push_graphics`] logs whatever it has to drop.
const MAX_APC_PAYLOAD: usize = MAX_TRANSMISSION_BASE64;

/// Cap on the reassembled base64 of one chunked transmission.
///
/// This has to stay under [`crate::daemon::protocol::MAX_FRAME`] once decoded:
/// base64 shrinks by 3/4, and [`Image::encode_frame`] prepends [`HEADER_LEN`]
/// bytes, so the decoded payload must leave room for the header inside the wire
/// frame. Blowing past `MAX_FRAME` would make `write_frame` fail, which the
/// daemon's writer loop treats as fatal — one oversized image would drop the
/// client's whole connection instead of just that frame. 48 MiB of base64 is
/// ~36 MB of pixels, comfortably more than a 4K full-window RGBA frame (~33 MB)
/// and comfortably under the 64 MiB wire ceiling.
const MAX_TRANSMISSION_BASE64: usize = 48 << 20; // 48 MiB

/// Cap on the *resolved* pixel bytes of one image, whatever medium carried it.
/// Bounds the file/shm fast path (where the sender names an object whose size we
/// don't control) and backstops the direct path, keeping every frame we hand the
/// daemon inside [`crate::daemon::protocol::MAX_FRAME`].
pub const MAX_IMAGE_BYTES: usize = crate::daemon::protocol::MAX_FRAME - HEADER_LEN;

/// A streaming tokenizer that splits raw output into a *passthrough* byte stream
/// and the kitty graphics commands lifted out of it.
///
/// Feed it raw output bytes. It invokes `on_passthrough` with every byte that is
/// **not** part of a `_G` graphics sequence — that is the exact input with each
/// `ESC _ G … ST` removed, and it is what the daemon appends to the replay ring
/// and forwards to the client. It invokes `on_command` with the complete payload
/// of each `_G` command (the bytes after `ESC _`, terminator excluded — e.g.
/// `Ga=T,f=32,…;<base64>`). Non-graphics APC sequences (any payload not starting
/// with `G`) are passed through verbatim, since only kitty graphics is ours to
/// intercept; everything else must reach the client's VT parser unchanged. State
/// persists across `feed` calls, so a sequence split over several reads is still
/// handled.
pub struct ApcTokenizer {
    /// Bytes of a `_G` command accumulated after `ESC _ G` while it can still
    /// terminate. Cleared whenever a command finishes or is abandoned.
    buf: Vec<u8>,
    state: State,
}

#[derive(Default, Clone, Copy)]
enum State {
    /// Not inside an escape sequence; bytes are passthrough.
    #[default]
    Ground,
    /// Held one `ESC` in ground state; a following `_` opens an APC.
    Esc,
    /// Held `ESC _`; the next byte decides graphics (`G`) vs. passthrough APC.
    ApcStart,
    /// Buffering a `_G` graphics command (stripped from passthrough).
    ApcGraphics,
    /// Saw `ESC` inside a graphics command — a following `\` is the terminator.
    ApcGraphicsEsc,
    /// Forwarding a non-graphics APC as passthrough (bytes kept verbatim).
    PassApc,
    /// Saw `ESC` inside a passthrough APC — a following `\` is the terminator.
    PassApcEsc,
    /// Dropping an abandoned/oversized graphics APC (emitted nowhere).
    ApcDrop,
    /// Saw `ESC` inside a dropped APC — a following `\` is the terminator.
    ApcDropEsc,
}

impl Default for ApcTokenizer {
    fn default() -> Self {
        Self::new()
    }
}

impl ApcTokenizer {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            state: State::Ground,
        }
    }

    /// Feed one chunk of output. `on_passthrough` receives runs of non-graphics
    /// bytes (the input minus every `ESC _ G … ST`); `on_command` receives the
    /// `G…` payload of each graphics command that terminates within the chunk.
    ///
    /// Like the OSC tokenizer, the states that dominate a real stream — `Ground`
    /// between sequences, and the scan-to-terminator states inside an APC — skip
    /// ahead with SIMD `memchr` rather than stepping per byte; only the few
    /// escape-decision states step one byte at a time. APC has no `BEL`
    /// terminator (kitty always closes with `ST`), so only `ESC` can end a
    /// payload. A held `ESC`/`ESC _` that later proves to be passthrough is
    /// re-emitted as a constant, so nothing needs to survive across `feed` calls
    /// but the small state tag and the graphics buffer.
    pub fn feed(
        &mut self,
        bytes: &[u8],
        mut on_passthrough: impl FnMut(&[u8]),
        mut on_command: impl FnMut(&[u8]),
    ) {
        let mut i = 0;
        while i < bytes.len() {
            match self.state {
                State::Ground => match memchr::memchr(0x1b, &bytes[i..]) {
                    Some(off) => {
                        if off > 0 {
                            on_passthrough(&bytes[i..i + off]);
                        }
                        self.state = State::Esc;
                        i += off + 1;
                    }
                    None => {
                        on_passthrough(&bytes[i..]);
                        return;
                    }
                },
                State::PassApc => match memchr::memchr(0x1b, &bytes[i..]) {
                    Some(off) => {
                        if off > 0 {
                            on_passthrough(&bytes[i..i + off]);
                        }
                        self.state = State::PassApcEsc;
                        i += off + 1;
                    }
                    None => {
                        on_passthrough(&bytes[i..]);
                        return;
                    }
                },
                State::ApcDrop => match memchr::memchr(0x1b, &bytes[i..]) {
                    Some(off) => {
                        self.state = State::ApcDropEsc;
                        i += off + 1;
                    }
                    None => return,
                },
                State::ApcGraphics => match memchr::memchr(0x1b, &bytes[i..]) {
                    Some(off) => {
                        if self.push_graphics(&bytes[i..i + off]) {
                            self.state = State::ApcGraphicsEsc;
                            i += off + 1;
                        } else {
                            // Oversized: abandon and let the drop state consume
                            // the terminator we just found (leave `i` on the ESC).
                            self.state = State::ApcDrop;
                            i += off;
                        }
                    }
                    None => {
                        if !self.push_graphics(&bytes[i..]) {
                            self.state = State::ApcDrop;
                        }
                        return;
                    }
                },
                State::Esc => match bytes[i] {
                    b'_' => {
                        self.buf.clear();
                        self.state = State::ApcStart;
                        i += 1;
                    }
                    // A run of ESCs: the earlier one was a lone ESC (passthrough);
                    // keep the newest one held.
                    0x1b => {
                        on_passthrough(b"\x1b");
                        i += 1;
                    }
                    // The ESC began some other escape: emit it and re-examine this
                    // byte from ground (it is not an ESC, so it joins the run).
                    _ => {
                        on_passthrough(b"\x1b");
                        self.state = State::Ground;
                    }
                },
                State::ApcStart => {
                    if bytes[i] == b'G' {
                        self.buf.clear();
                        self.buf.push(b'G');
                        self.state = State::ApcGraphics;
                        i += 1;
                    } else {
                        // Not ours: emit the held `ESC _` and forward the rest of
                        // the APC verbatim (re-examine this byte in `PassApc`).
                        on_passthrough(b"\x1b_");
                        self.state = State::PassApc;
                    }
                }
                State::ApcGraphicsEsc => match bytes[i] {
                    b'\\' => {
                        on_command(&self.buf);
                        self.buf.clear();
                        self.state = State::Ground;
                        i += 1;
                    }
                    0x1b => i += 1, // a run of ESCs; stay poised for `\`
                    // A bare `ESC _` re-opens: the next byte decides graphics again.
                    b'_' => {
                        self.buf.clear();
                        self.state = State::ApcStart;
                        i += 1;
                    }
                    // ESC began some other escape: abandon this graphics command.
                    // Its bytes were meant as graphics, so they stay stripped —
                    // but the escape itself belongs to the terminal, so forward
                    // it and re-examine this byte from Ground rather than eating
                    // both. Swallowing them would turn a `\x1b[31m` that follows
                    // an unterminated graphics command into literal `31m`.
                    _ => {
                        self.buf.clear();
                        on_passthrough(b"\x1b");
                        self.state = State::Ground;
                    }
                },
                State::PassApcEsc => match bytes[i] {
                    b'\\' => {
                        on_passthrough(b"\x1b\\"); // ST is part of the forwarded APC
                        self.state = State::Ground;
                        i += 1;
                    }
                    0x1b => {
                        on_passthrough(b"\x1b"); // a lone ESC in APC data
                        i += 1;
                    }
                    // A bare `ESC _` re-opens, same as the two graphics states
                    // do. Without this, a foreign APC that never sends its ST
                    // swallows every `ESC _G …` after it — the graphics get
                    // forwarded as APC text that the client's vte then discards,
                    // so the image is simply lost.
                    b'_' => {
                        self.buf.clear();
                        self.state = State::ApcStart;
                        i += 1;
                    }
                    _ => {
                        on_passthrough(b"\x1b");
                        self.state = State::PassApc; // re-examine this byte
                    }
                },
                State::ApcDropEsc => match bytes[i] {
                    b'\\' => {
                        self.state = State::Ground;
                        i += 1;
                    }
                    0x1b => i += 1,
                    b'_' => {
                        self.buf.clear();
                        self.state = State::ApcStart;
                        i += 1;
                    }
                    // As in `ApcGraphicsEsc`: the dropped command's bytes stay
                    // stripped, but the escape that interrupted it is the
                    // terminal's and has to reach the client intact.
                    _ => {
                        on_passthrough(b"\x1b");
                        self.state = State::Ground;
                    }
                },
            }
        }
    }

    /// Append a run to the graphics buffer; returns `false` (and clears the
    /// buffer) if it would exceed [`MAX_APC_PAYLOAD`], signalling the caller to
    /// abandon the command.
    fn push_graphics(&mut self, run: &[u8]) -> bool {
        if self.buf.len() + run.len() > MAX_APC_PAYLOAD {
            log::debug!(
                "kitty graphics: dropping a command whose payload passed {MAX_APC_PAYLOAD} bytes"
            );
            self.buf.clear();
            return false;
        }
        self.buf.extend_from_slice(run);
        true
    }
}

/// Daemon-side splitter: the [`ApcTokenizer`] wired to a [`GraphicsParser`].
///
/// This is the one type the PTY reader loop touches. Feed it raw output and a
/// passthrough sink; it strips every kitty graphics sequence, funnels the
/// passthrough bytes to the sink (for the replay ring and the client), and
/// returns the decoded graphics [`Event`]s — query replies to write back to the
/// PTY, and images to forward out-of-band.
#[derive(Default)]
pub struct GraphicsSniffer {
    tokenizer: ApcTokenizer,
    parser: GraphicsParser,
}

impl GraphicsSniffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A sniffer whose parser may honor file/shm transfer (local pane only).
    /// See [`GraphicsParser::new_local`].
    pub fn new_local(local: bool) -> Self {
        Self {
            tokenizer: ApcTokenizer::new(),
            parser: GraphicsParser::new_local(local),
        }
    }

    /// Update whether the sender currently shares this host's filesystem. A pane
    /// is local when tty7 spawned a local shell and no foreground `ssh` owns the
    /// PTY; that can flip mid-session, and it gates whether the next `a=q` probe
    /// is answered `OK` for file/shm transfer. Cheap enough to call per chunk.
    pub fn set_local(&mut self, local: bool) {
        self.parser.local = local;
    }

    /// Feed one chunk of raw PTY output. `on_passthrough` receives the byte
    /// stream with all graphics sequences removed; the returned events are the
    /// queries/images/deletes that completed within this chunk, in order.
    ///
    /// This drops the *relative order* of passthrough vs. events; for the daemon
    /// loop, prefer [`sniff`](Self::sniff), which preserves it. Retained as the
    /// low-level primitive the unit tests drive.
    pub fn feed(&mut self, bytes: &[u8], on_passthrough: impl FnMut(&[u8])) -> Vec<Event> {
        let Self { tokenizer, parser } = self;
        let mut events = Vec::new();
        tokenizer.feed(bytes, on_passthrough, |cmd| {
            if let Some(ev) = parser.feed(cmd) {
                events.push(ev);
            }
        });
        events
    }

    /// Feed a chunk and get back its content **in stream order**, taking a
    /// zero-copy fast path on the overwhelmingly common chunk that carries no
    /// graphics at all.
    ///
    /// Order matters: a kitty image is placed at the cursor cell as it stood
    /// *when its command appeared in the stream*, so the client must apply the
    /// text before an image before resolving that image's anchor. Returning
    /// ordered [`Segment`]s lets the daemon forward `Output`/`Image` frames
    /// interleaved exactly as they arrived, so the client's mirror cursor is
    /// correct at each placement.
    ///
    /// The fast path fires when the tokenizer is between sequences *and* the
    /// chunk holds no APC opener (`ESC _`) — then the input **is** one big
    /// output segment, returned borrowed with no allocation. A build's worth of
    /// colored stdout (full of CSI escapes but no APC) stays on this path. The
    /// trailing-`ESC` guard keeps an `ESC _` straddling a chunk boundary on the
    /// slow path, where the held-`ESC` state carries it across correctly.
    pub fn sniff<'a>(&mut self, bytes: &'a [u8]) -> Sniffed<'a> {
        if matches!(self.tokenizer.state, State::Ground)
            && bytes.last() != Some(&0x1b)
            && memchr::memmem::find(bytes, b"\x1b_").is_none()
        {
            return Sniffed::Plain(bytes);
        }
        let Self { tokenizer, parser } = self;
        // Both tokenizer callbacks push onto the same segment list; `RefCell`
        // lets the passthrough and command sinks share it without either taking
        // a lasting mutable borrow the borrow checker would reject.
        let segments: std::cell::RefCell<Vec<Segment>> = std::cell::RefCell::new(Vec::new());
        tokenizer.feed(
            bytes,
            |run| {
                // Coalesce adjacent passthrough runs into one output segment so
                // the client applies them in a single `advance`.
                let mut segs = segments.borrow_mut();
                if let Some(Segment::Output(buf)) = segs.last_mut() {
                    buf.extend_from_slice(run);
                } else {
                    segs.push(Segment::Output(run.to_vec()));
                }
            },
            |cmd| {
                if let Some(ev) = parser.feed(cmd) {
                    segments.borrow_mut().push(Segment::from(ev));
                }
            },
        );
        Sniffed::Segments(segments.into_inner())
    }
}

/// One ordered piece of a sniffed chunk — see [`GraphicsSniffer::sniff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Non-graphics bytes to append to the ring / forward as `Output`.
    Output(Vec<u8>),
    /// An `a=q` reply to write back to the PTY.
    Query(Vec<u8>),
    /// An image to forward out-of-band.
    Image(Image),
    /// A file/shm transmission the daemon must resolve into pixels before
    /// forwarding (local panes only). See [`MediumTransfer`].
    ImageFromMedium(MediumTransfer),
    /// A delete to forward out-of-band.
    Delete(ImageDelete),
}

impl From<Event> for Segment {
    fn from(ev: Event) -> Self {
        match ev {
            Event::Query { reply, .. } => Segment::Query(reply),
            Event::Image(img) => Segment::Image(img),
            Event::ImageFromMedium(t) => Segment::ImageFromMedium(t),
            Event::Delete(c) => Segment::Delete(ImageDelete::from_control(&c)),
        }
    }
}

/// The result of [`GraphicsSniffer::sniff`]: either the whole chunk borrowed as
/// output (the graphics-free fast path), or ordered [`Segment`]s.
pub enum Sniffed<'a> {
    /// No graphics in this chunk: the input is output, verbatim and borrowed.
    Plain(&'a [u8]),
    /// Graphics present: apply these in order.
    Segments(Vec<Segment>),
}

/// The kitty action key (`a=`). Only the variants tty7 acts on are named; the
/// rest of the protocol (frames, animation, compose) folds into `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `a=q` — query whether a transmission would succeed. We must reply.
    Query,
    /// `a=t` — transmit only (no display). Rare from our target sender.
    Transmit,
    /// `a=T` — transmit and display.
    TransmitAndDisplay,
    /// `a=p` — display a previously transmitted image.
    Display,
    /// `a=d` — delete image(s)/placement(s).
    Delete,
    /// Any action we don't specifically handle.
    Other,
}

/// The pixel format key (`f=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireFormat {
    /// `f=24` — 3 bytes/pixel RGB.
    Rgb,
    /// `f=32` — 4 bytes/pixel RGBA (the default, and what `terminal-browser` uses).
    Rgba,
    /// `f=100` — a PNG file; pixels stay encoded (decoded client-side).
    Png,
}

/// The transmission medium key (`t=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    /// `t=d` — direct: the payload is (base64) the image data itself.
    Direct,
    /// `t=f` — a filesystem path to the data.
    File,
    /// `t=s` — a POSIX shared-memory object name.
    Shared,
    /// `t=t` — a temporary file (deleted after reading).
    TempFile,
}

/// Parsed kitty graphics control keys (the `k=v,k=v` list before the `;`).
///
/// Only the keys tty7 needs are surfaced; unknown keys are ignored so forward
/// protocol additions degrade quietly rather than failing the whole command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Control {
    pub action: Action,
    pub format: WireFormat,
    pub medium: Medium,
    /// `o=z` — payload is zlib-compressed.
    pub compressed: bool,
    /// `i=` — client-assigned image id (0 = unset).
    pub id: u32,
    /// `I=` — client-assigned image number (0 = unset).
    pub number: u32,
    /// `p=` — placement id (0 = unset).
    pub placement: u32,
    /// `s=` — source pixel width.
    pub width: u32,
    /// `v=` — source pixel height.
    pub height: u32,
    /// `c=` — columns to display across (0 = derive from pixels).
    pub cols: u32,
    /// `r=` — rows to display down (0 = derive from pixels).
    pub rows: u32,
    /// `m=1` — more chunks follow.
    pub more: bool,
    /// `d=` — delete target (only meaningful for `a=d`).
    pub delete: u8,
    /// `q=` — suppress responses (1 = errors only, 2 = all).
    pub quiet: u8,
    /// `O=` — byte offset into a file/shm object (file/shm mediums only).
    pub offset: u32,
    /// `S=` — number of bytes to read from a file/shm object (0 = to end).
    pub size: u32,
}

impl Default for Control {
    fn default() -> Self {
        // Protocol defaults: a=t, f=32, t=d, no compression.
        Self {
            action: Action::Transmit,
            format: WireFormat::Rgba,
            medium: Medium::Direct,
            compressed: false,
            id: 0,
            number: 0,
            placement: 0,
            width: 0,
            height: 0,
            cols: 0,
            rows: 0,
            more: false,
            delete: 0,
            quiet: 0,
            offset: 0,
            size: 0,
        }
    }
}

impl Control {
    /// Parse the control section of a `_G` command — the `G…` payload up to (but
    /// not including) the first `;`. The leading `G` is part of the first key
    /// name's position, i.e. the payload is `Ga=T,f=32,…;<data>`; we accept the
    /// whole `G…;…` slice and split on the `;`.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        // Strip the leading `G` identifier.
        let rest = payload.strip_prefix(b"G")?;
        let control = match rest.iter().position(|&b| b == b';') {
            Some(pos) => &rest[..pos],
            None => rest, // no payload section (e.g. `a=d`)
        };
        let mut c = Control::default();
        for pair in control.split(|&b| b == b',') {
            if pair.is_empty() {
                continue;
            }
            let mut kv = pair.splitn(2, |&b| b == b'=');
            let key = kv.next().unwrap_or(b"");
            let val = kv.next().unwrap_or(b"");
            let num = || parse_u32(val);
            match key {
                b"a" => {
                    c.action = match val {
                        b"q" => Action::Query,
                        b"t" => Action::Transmit,
                        b"T" => Action::TransmitAndDisplay,
                        b"p" => Action::Display,
                        b"d" => Action::Delete,
                        _ => Action::Other,
                    }
                }
                b"f" => {
                    c.format = match val {
                        b"24" => WireFormat::Rgb,
                        b"100" => WireFormat::Png,
                        _ => WireFormat::Rgba, // 32 and the default
                    }
                }
                b"t" => {
                    c.medium = match val {
                        b"f" => Medium::File,
                        b"s" => Medium::Shared,
                        b"t" => Medium::TempFile,
                        _ => Medium::Direct,
                    }
                }
                b"o" => c.compressed = val == b"z",
                b"i" => c.id = num(),
                b"I" => c.number = num(),
                b"p" => c.placement = num(),
                b"s" => c.width = num(),
                b"v" => c.height = num(),
                b"c" => c.cols = num(),
                b"r" => c.rows = num(),
                b"m" => c.more = val == b"1",
                b"d" => c.delete = val.first().copied().unwrap_or(0),
                // Clamp rather than truncate: `q=256` must not wrap to 0 and
                // turn a request for silence into a request for chatter.
                b"q" => c.quiet = num().min(u8::MAX as u32) as u8,
                b"O" => c.offset = num(),
                b"S" => c.size = num(),
                _ => {} // unknown key: ignore
            }
        }
        Some(c)
    }
}

fn parse_u32(bytes: &[u8]) -> u32 {
    let mut n: u32 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return 0;
        }
        n = n.saturating_mul(10).saturating_add((b - b'0') as u32);
    }
    n
}

/// The `;`-separated data section of a `_G` command (may be empty).
fn payload_data(command: &[u8]) -> &[u8] {
    match command.iter().position(|&b| b == b';') {
        Some(pos) => &command[pos + 1..],
        None => &[],
    }
}

/// A reassembled image transmission, base64-decoded but *not yet* inflated.
///
/// The daemon does only the base64 decode (cheap, and it strips the 33% text
/// inflation) and forwards this over the socket; the client calls [`to_rgba8`]
/// to inflate and normalize to RGBA8. Keeping [`data`] compressed on the wire is
/// what makes a *remote* pane viable — inflating daemon-side would push tens of
/// MB per frame back through the SSH tunnel.
///
/// [`data`]: Image::data
/// [`to_rgba8`]: Image::to_rgba8
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub id: u32,
    pub number: u32,
    pub placement: u32,
    /// Source pixel dimensions.
    pub width: u32,
    pub height: u32,
    /// Requested display span in cells (0 = derive from pixels / cell size).
    pub cols: u32,
    pub rows: u32,
    /// The base64-decoded payload: zlib-compressed pixels when [`compressed`],
    /// otherwise raw pixels ([`WireFormat::Rgb`]/[`WireFormat::Rgba`]) or an
    /// encoded PNG file ([`WireFormat::Png`]).
    ///
    /// [`compressed`]: Image::compressed
    pub data: Vec<u8>,
    pub format: WireFormat,
    /// Whether [`data`](Image::data) is still zlib-compressed (`o=z`).
    pub compressed: bool,
}

impl Image {
    /// Inflate and normalize to tightly packed RGBA8 (`width*height*4`), the
    /// form the renderer wants. Runs client-side. Returns `None` for a PNG
    /// (whose decode needs the `image` crate the GUI owns, not this core) or on
    /// a malformed payload; PNG callers should decode [`data`](Image::data)
    /// themselves after checking [`format`](Image::format).
    ///
    /// The inflate is bounded by what `width`/`height` claim the pixels are:
    /// deflate expands ~1000:1 in the limit, so an unbounded `decompress_to_vec`
    /// here would let a payload well inside [`MAX_TRANSMISSION_BASE64`] balloon
    /// into tens of GB and OOM the client — and this runs in the GUI process, so
    /// it would take every pane down, not just the one that received the escape.
    /// The caller checks the inflated length against `width*height*4` anyway, so
    /// capping it up front costs nothing.
    pub fn to_rgba8(&self) -> Option<Vec<u8>> {
        if self.format == WireFormat::Png {
            return None;
        }
        let raw = if self.compressed {
            // Bound the inflate by the pixels the sender *claims* to be sending
            // (`width * height` at this format's bytes-per-pixel). Without a cap,
            // a high-ratio deflate payload well inside `MAX_TRANSMISSION_BASE64`
            // (which bounds only the *compressed* bytes) inflates to tens of GB
            // and OOMs the whole GUI process. A payload that decompresses past
            // its own declared dimensions is malformed, so we drop it.
            //
            // The declared size is itself attacker-chosen, so clamp it too:
            // `s=65535,v=65535` alone works out to 17 GB, which would hand the
            // bomb right back its allocation. No real frame comes near
            // `MAX_IMAGE_BYTES`, which is what the wire can carry anyway.
            let limit = self.decoded_len()?.min(MAX_IMAGE_BYTES);
            miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&self.data, limit).ok()?
        } else {
            self.data.clone()
        };
        Some(match self.format {
            WireFormat::Rgb => rgb_to_rgba(&raw),
            _ => raw, // Rgba
        })
    }

    /// The byte length of this image's *decoded* (pre-`rgb_to_rgba`) pixels —
    /// `width * height * bytes_per_pixel` for the wire format. `None` on overflow
    /// or a zero-sized/PNG image, which have no fixed raw length.
    fn decoded_len(&self) -> Option<usize> {
        let bpp = match self.format {
            WireFormat::Rgb => 3usize,
            WireFormat::Rgba => 4usize,
            WireFormat::Png => return None,
        };
        (self.width as usize)
            .checked_mul(self.height as usize)?
            .checked_mul(bpp)
            .filter(|&n| n != 0)
    }

    /// Encode for the daemon→client [`crate::daemon::protocol::DaemonMsg::Image`]
    /// frame: a fixed 30-byte header carrying the metadata, then the raw `data`
    /// bytes appended verbatim. A JSON envelope would base64-inflate the pixel
    /// payload ~1.33×; this keeps it byte-for-byte, which matters at video frame
    /// rates. See [`decode_frame`](Image::decode_frame).
    pub fn encode_frame(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.data.len());
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.number.to_le_bytes());
        out.extend_from_slice(&self.placement.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.cols.to_le_bytes());
        out.extend_from_slice(&self.rows.to_le_bytes());
        out.push(match self.format {
            WireFormat::Rgb => 24,
            WireFormat::Rgba => 32,
            WireFormat::Png => 100,
        });
        out.push(u8::from(self.compressed));
        out.extend_from_slice(&self.data);
        out
    }

    /// Reconstruct from an [`encode_frame`](Image::encode_frame) payload.
    pub fn decode_frame(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER_LEN {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        let format = match bytes[28] {
            24 => WireFormat::Rgb,
            100 => WireFormat::Png,
            _ => WireFormat::Rgba,
        };
        Some(Image {
            id: u32_at(0),
            number: u32_at(4),
            placement: u32_at(8),
            width: u32_at(12),
            height: u32_at(16),
            cols: u32_at(20),
            rows: u32_at(24),
            format,
            compressed: bytes[29] != 0,
            data: bytes[HEADER_LEN..].to_vec(),
        })
    }
}

/// Byte length of the [`Image::encode_frame`] header: seven `u32` fields, then
/// the format and compression bytes.
const HEADER_LEN: usize = 7 * 4 + 1 + 1;

impl MediumTransfer {
    /// Resolve the referenced bytes into an [`Image`] by reading the file or
    /// `mmap`ing the POSIX shm object this transfer names, then unlinking it (a
    /// [`Medium::TempFile`] and any shm object are one-shot handoffs the sender
    /// expects the terminal to consume and remove; a plain [`Medium::File`] is
    /// left in place). Runs on the daemon reader thread of a *local* pane, where
    /// the name resolves on this host. Returns `None` if the name is unusable or
    /// the object can't be read.
    ///
    /// The pixels come back *uncompressed* unless the sender set `o=z`: a sender
    /// that reaches for shared memory does so precisely to avoid the zlib the
    /// inline path forces, so this is the fast path that skips the client-side
    /// inflate entirely.
    #[cfg(unix)]
    pub fn resolve(&self) -> Option<Image> {
        let data = match self.medium {
            Medium::Shared => self.read_shared()?,
            Medium::File | Medium::TempFile => self.read_file()?,
            // Direct never becomes a MediumTransfer.
            Medium::Direct => return None,
        };
        Some(Image {
            id: self.id,
            number: self.number,
            placement: self.placement,
            width: self.width,
            height: self.height,
            cols: self.cols,
            rows: self.rows,
            data,
            format: self.format,
            compressed: self.compressed,
        })
    }

    #[cfg(not(unix))]
    pub fn resolve(&self) -> Option<Image> {
        None
    }

    /// The `offset..offset+size` slice of `buf` (or `offset..` when `size == 0`),
    /// copied out. `None` if `offset` itself falls outside the object; an `S=`
    /// that runs past the end is *truncated* to what's there rather than
    /// discarding the frame, which is what kitty does.
    #[cfg(unix)]
    fn slice_region(&self, buf: &[u8]) -> Option<Vec<u8>> {
        let start = self.offset as usize;
        if start > buf.len() {
            return None;
        }
        let end = if self.size == 0 {
            buf.len()
        } else {
            start.saturating_add(self.size as usize).min(buf.len())
        };
        buf.get(start..end).map(<[u8]>::to_vec)
    }

    #[cfg(unix)]
    fn read_file(&self) -> Option<Vec<u8>> {
        use std::io::Read as _;
        use std::os::unix::ffi::OsStringExt;
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(self.name.clone()));

        // Read through a bounded reader, not `fs::read`. The name is attacker
        // -reachable (any program that can write to the pty picks it), and
        // `fs::read` on `/dev/zero` never returns — on the daemon *reader*
        // thread, which would wedge the pane's whole output path. Refusing
        // anything that isn't a regular file also keeps us off fifos and
        // devices, where the open itself can block.
        let file = std::fs::File::open(&path).ok()?;
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.len() as usize > MAX_IMAGE_BYTES {
            return None;
        }
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        file.take(MAX_IMAGE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return None;
        }

        let out = self.slice_region(&bytes)?;
        // A temp file is the sender's one-shot handoff: remove it after reading
        // so the browser's per-frame temp files don't pile up. A named `t=f`
        // file is the sender's to manage; leave it.
        //
        // Only unlink inside a known temp directory. `name` is an arbitrary path
        // out of an escape sequence — `cat`ing a hostile file is enough to send
        // one — so an unqualified `remove_file` here would delete anything the
        // user can, `~/.ssh/id_ed25519` included. The spec requires this check
        // for exactly that reason.
        if self.medium == Medium::TempFile && path_is_in_temp_dir(&path) {
            let _ = std::fs::remove_file(&path);
        }
        Some(out)
    }

    /// `shm_open` + `mmap` the object, copy out the requested region, then
    /// `shm_unlink` it (the sender allocates a fresh object per frame and
    /// expects the terminal to reclaim it — matching kitty/ghostty).
    #[cfg(unix)]
    fn read_shared(&self) -> Option<Vec<u8>> {
        use std::os::raw::c_void;
        // A POSIX shm name is a single `/`-prefixed component — no embedded
        // separators, no `..`. Some platforms resolve `shm_open` against the
        // filesystem, where a name like `/../../etc/passwd` would escape the shm
        // namespace and reach a real path we'd then `shm_unlink`.
        if !shm_name_is_wellformed(&self.name) {
            return None;
        }
        // The name must be a C string; kitty shm names look like `/px-…`.
        let cname = std::ffi::CString::new(self.name.clone()).ok()?;
        // SAFETY: FFI. `shm_open` with O_RDONLY on a name the sender created;
        // we only ever read, mmap read-only, and always munmap/close/unlink on
        // every exit path below.
        unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0);
            if fd < 0 {
                return None;
            }
            // Size the mapping from the object itself; the sender may not send
            // `S=`, and mapping past the end would fault on access.
            // Refuse an object bigger than a frame can carry rather than mapping
            // and copying it out only for the send to fail downstream.
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut st) != 0
                || st.st_size <= 0
                || st.st_size as u64 > MAX_IMAGE_BYTES as u64
            {
                libc::close(fd);
                libc::shm_unlink(cname.as_ptr());
                return None;
            }
            let len = st.st_size as usize;
            let addr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            );
            libc::close(fd);
            if addr == libc::MAP_FAILED {
                libc::shm_unlink(cname.as_ptr());
                return None;
            }
            let mapped = std::slice::from_raw_parts(addr as *const u8, len);
            let out = self.slice_region(mapped);
            libc::munmap(addr as *mut c_void, len);
            // One-shot: the sender expects us to reclaim the object.
            libc::shm_unlink(cname.as_ptr());
            out
        }
    }
}

/// Whether `path` sits inside a directory the platform hands out for temp files,
/// which is the only place a `t=t` handoff may be unlinked.
///
/// Compares *canonicalized* paths so `/tmp/../home/me/.ssh/id_ed25519` — which
/// has the right prefix textually — doesn't pass. On macOS `TMPDIR` is a
/// per-user path under `/var/folders/…` that canonicalizes through the
/// `/private` symlink, so `std::env::temp_dir()` is canonicalized too rather
/// than compared raw.
#[cfg(unix)]
fn path_is_in_temp_dir(path: &std::path::Path) -> bool {
    let Ok(real) = path.canonicalize() else {
        return false;
    };
    // `/dev/shm` is where a `t=t` sender that wanted shm-like semantics without
    // `shm_open` puts its handoff; kitty accepts it alongside the temp dirs.
    let candidates = [
        std::env::temp_dir(),
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/dev/shm"),
    ];
    candidates.iter().any(|dir| {
        dir.canonicalize()
            .is_ok_and(|d| real.starts_with(&d) && real != d)
    })
}

/// Whether `name` is a well-formed POSIX shm object name: a leading `/` followed
/// by one non-empty component with no further separators and no `.`/`..`.
#[cfg(unix)]
fn shm_name_is_wellformed(name: &[u8]) -> bool {
    // POSIX caps the name at NAME_MAX; 255 is the floor every platform we build
    // for meets, and no real sender comes close.
    if name.len() < 2 || name.len() > 255 || name[0] != b'/' {
        return false;
    }
    let rest = &name[1..];
    !rest.contains(&b'/') && !rest.contains(&0) && rest != b"." && rest != b".."
}

/// A delete request distilled from an `a=d` command, in the compact form the
/// daemon forwards to the client's image store. The full kitty delete grammar is
/// rich (by id, by placement, by cell, by z-index, …); the client only needs the
/// target selector plus the id/placement it may scope to, which is all any sender
/// tty7 targets — most often `d=A` (delete everything) — actually uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDelete {
    /// The `d=` selector byte (e.g. `A`/`a` = all, `i` = by id, `p` = by
    /// placement). Uppercase variants also free the image data in kitty; the
    /// client frees unconditionally, so case only affects which images match.
    pub target: u8,
    pub id: u32,
    pub placement: u32,
}

impl ImageDelete {
    pub fn from_control(c: &Control) -> Self {
        Self {
            // A bare `a=d` with no `d=` means "delete all visible placements",
            // which kitty spells `a` — normalize the unset case to it.
            target: if c.delete == 0 { b'a' } else { c.delete },
            id: c.id,
            placement: c.placement,
        }
    }

    /// A fixed 9-byte frame: the selector byte then id and placement (LE).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(9);
        out.push(self.target);
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.placement.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 9 {
            return None;
        }
        Some(Self {
            target: bytes[0],
            id: u32::from_le_bytes(bytes[1..5].try_into().unwrap()),
            placement: u32::from_le_bytes(bytes[5..9].try_into().unwrap()),
        })
    }
}

/// What a fed command turned into. Query/Delete carry no pixels; the caller
/// writes the query reply back to the PTY and applies deletes to its store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// An `a=q` probe. `reply` is the bytes to write to the PTY; `honored` says
    /// whether we accepted the requested medium (for logging/metrics only).
    Query { reply: Vec<u8>, honored: bool },
    /// A complete image transmission (`a=T`/`a=t`).
    Image(Image),
    /// A transmission whose pixels live in a file or POSIX shm object rather
    /// than inline in the escape. The parser can't read the filesystem (it must
    /// stay pure and host-agnostic), so it hands the reference to the daemon
    /// pane — which is co-located with the sender on a *local* pane — to `mmap`
    /// / read and unlink. This is the zero-copy, zero-inflate path a sender like
    /// `terminal-browser` prefers; see [`MediumTransfer`].
    ImageFromMedium(MediumTransfer),
    /// An `a=d` delete request.
    Delete(Control),
}

/// A file/shm transmission the daemon must resolve into pixels. The `name` is
/// the (base64-decoded) payload the sender put after the `;`: a filesystem path
/// for [`Medium::File`]/[`Medium::TempFile`], or a POSIX shm object name for
/// [`Medium::Shared`]. `offset`/`size` bound the region to read (`size == 0`
/// means "to end of object"). All the metadata needed to build the final
/// [`Image`] rides along so the daemon does no parsing of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediumTransfer {
    pub medium: Medium,
    pub name: Vec<u8>,
    pub offset: u32,
    pub size: u32,
    pub id: u32,
    pub number: u32,
    pub placement: u32,
    pub width: u32,
    pub height: u32,
    pub cols: u32,
    pub rows: u32,
    pub format: WireFormat,
    /// Whether the referenced bytes are zlib-compressed (`o=z`). A sender that
    /// picks shm/file for speed sends raw pixels, but honor the flag regardless.
    pub compressed: bool,
}

/// State for reassembling a chunked (`m=1`) direct transmission. Kitty
/// serializes chunked transmissions — only one is in flight at a time — so a
/// single pending accumulator suffices.
#[derive(Default)]
struct Pending {
    control: Control,
    /// Accumulated *base64* text across chunks (decoded once, at the end).
    base64: Vec<u8>,
}

/// Reassembles and decodes kitty graphics commands emitted by [`ApcTokenizer`].
///
/// Feed it each `_G` command payload; it returns zero or more [`Event`]s. It
/// owns the chunk-reassembly buffer and the base64/zlib decode, so the daemon
/// pane just forwards the resulting [`Image`] out-of-band and writes any
/// [`Event::Query`] reply to the PTY.
#[derive(Default)]
pub struct GraphicsParser {
    pending: Option<Pending>,
    /// Whether the sender shares this host's filesystem (a local pane). Only
    /// then can we honor file/shm transfer, whose names are host-local; a pane
    /// running over SSH must stay on inline `t=d` so the pixels ride the tunnel.
    local: bool,
}

impl GraphicsParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// A parser that may honor file/shm transfer because the sender is on this
    /// host (see [`GraphicsParser::local`]).
    pub fn new_local(local: bool) -> Self {
        Self {
            local,
            ..Self::default()
        }
    }

    /// Whether a `t=f`/`t=t`/`t=s` transfer can actually be resolved here: the
    /// sender has to share this host's filesystem (a local pane), and
    /// [`MediumTransfer::resolve`] has to have a real implementation on this
    /// platform (it is unix-only). Both `query_reply` and `finalize` route
    /// through this so what we advertise and what we accept can't drift apart.
    fn honors_indirect_media(&self) -> bool {
        self.local && cfg!(unix)
    }

    /// Feed one complete `_G` command payload (the bytes [`ApcTokenizer`]
    /// delivers). Returns an [`Event`] when a command completes (a query, a
    /// finished image, or a delete); returns `None` for an intermediate chunk of
    /// a still-incomplete transmission.
    pub fn feed(&mut self, command: &[u8]) -> Option<Event> {
        let control = Control::parse(command)?;
        let data = payload_data(command);

        // A continuation chunk of a chunked transmission carries only `m=…` — no
        // `a=` — so its action defaults to `Transmit`. Route anything while a
        // transmission is pending straight to the chunk accumulator, which keeps
        // the *first* chunk's real control keys.
        if self.pending.is_some() {
            return self.accept_chunk(control, data);
        }

        match control.action {
            Action::Query => self.query_reply(&control),
            Action::Delete => Some(Event::Delete(control)),
            Action::TransmitAndDisplay | Action::Transmit => self.accept_chunk(control, data),
            // `a=p` places an image transmitted by an *earlier* command, which
            // needs the stored-image table this parser deliberately doesn't
            // keep. Routing it through `accept_chunk` would emit an `Image` with
            // an empty payload — a frame the client can only throw away — so
            // drop it here instead. Senders that split transmit from placement
            // get nothing; `a=T` (the shape every sender tty7 targets uses)
            // is unaffected.
            Action::Display => None,
            // An action we don't handle, with nothing in flight: drop it.
            Action::Other => None,
        }
    }

    /// Append a (possibly first) chunk; finalize when `m` is not set.
    fn accept_chunk(&mut self, control: Control, data: &[u8]) -> Option<Event> {
        // The first chunk carries the real control keys; later chunks repeat
        // only `m=`. Preserve the first chunk's control across the transmission.
        let is_first = self.pending.is_none();
        if is_first {
            self.pending = Some(Pending {
                control,
                base64: Vec::new(),
            });
        }
        let pending = self.pending.as_mut()?;
        pending.base64.extend_from_slice(data);
        if pending.base64.len() > MAX_TRANSMISSION_BASE64 {
            log::debug!(
                "kitty graphics: abandoning a transmission past {MAX_TRANSMISSION_BASE64} base64 bytes"
            );
            self.pending = None; // abandon an oversized transmission
            return None;
        }
        if control.more {
            return None; // more chunks to come
        }
        // Complete: decode and emit.
        let Pending { control, base64 } = self.pending.take()?;
        self.finalize(control, &base64)
    }

    fn finalize(&self, control: Control, base64: &[u8]) -> Option<Event> {
        // Base64-decode the payload once. For direct transmission this *is* the
        // (still-compressed) pixels; for file/shm it's the path/object name.
        let data = BASE64.decode(base64).ok()?;

        // File/shm transfer: the payload names bytes on this host. Hand the
        // reference to the daemon to resolve — but only on a local pane, where
        // the name is meaningful and reading it can't leak across an SSH tunnel.
        // A `query_reply` earlier already refused these mediums on a remote
        // pane, so a well-behaved sender never reaches here; the guard is
        // belt-and-suspenders.
        if control.medium != Medium::Direct {
            if !self.honors_indirect_media() {
                return None;
            }
            return Some(Event::ImageFromMedium(MediumTransfer {
                medium: control.medium,
                name: data,
                offset: control.offset,
                size: control.size,
                id: control.id,
                number: control.number,
                placement: control.placement,
                width: control.width,
                height: control.height,
                cols: control.cols,
                rows: control.rows,
                format: control.format,
                compressed: control.compressed,
            }));
        }

        // Direct: inflation is deferred to the client (`to_rgba8`) so the
        // payload rides the socket — and any SSH tunnel — compressed.
        Some(Event::Image(Image {
            id: control.id,
            number: control.number,
            placement: control.placement,
            width: control.width,
            height: control.height,
            cols: control.cols,
            rows: control.rows,
            data,
            format: control.format,
            compressed: control.compressed,
        }))
    }

    /// Build the `a=q` reply. On a local unix pane we honor direct, file, and
    /// shm transfer, so any of those probes gets `OK`; otherwise only direct is
    /// honored and a `t=f`/`t=s`/`t=t` probe gets an error — which makes a
    /// sender like `terminal-browser` fall back to inline `t=d` on its own.
    ///
    /// The answer has to track what [`MediumTransfer::resolve`] can actually do,
    /// including on the platform axis: it is unix-only, so replying `OK` to a
    /// file/shm probe on Windows would talk a sender into a medium whose every
    /// frame we then silently discard, leaving the pane blank.
    fn query_reply(&self, control: &Control) -> Option<Event> {
        let honored = control.medium == Medium::Direct || self.honors_indirect_media();
        // `q=` asks us to stay quiet: 1 suppresses success replies, 2 suppresses
        // failures too. This matters because the reply is written back to the
        // *PTY* — it arrives as if the user had typed it. A sender that asked
        // for silence and gets `\x1b_Gi=1;OK\x1b\` anyway has those bytes land
        // in its stdin, or, if it already exited, on the shell's input line.
        if control.quiet >= 2 || (control.quiet == 1 && honored) {
            return None;
        }
        let status: &[u8] = if honored { b"OK" } else { b"ENOTSUPPORTED" };
        // Echo back the id (preferred) or number the sender used, exactly as
        // kitty does, so the sender can correlate the reply.
        let mut reply = Vec::with_capacity(32);
        reply.extend_from_slice(b"\x1b_G");
        if control.id != 0 {
            reply.extend_from_slice(format!("i={}", control.id).as_bytes());
        } else if control.number != 0 {
            reply.extend_from_slice(format!("I={}", control.number).as_bytes());
        } else {
            reply.extend_from_slice(b"i=0");
        }
        reply.push(b';');
        reply.extend_from_slice(status);
        reply.extend_from_slice(b"\x1b\\");
        Some(Event::Query { reply, honored })
    }
}

/// Expand tightly packed RGB into RGBA with an opaque alpha channel.
fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
    for px in rgb.chunks_exact(3) {
        out.extend_from_slice(px);
        out.push(0xff);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut tok = ApcTokenizer::new();
        let mut out = Vec::new();
        for c in chunks {
            tok.feed(c, |_| {}, |cmd| out.push(cmd.to_vec()));
        }
        out
    }

    /// Feed `chunks` and return the concatenated passthrough stream (input with
    /// every `_G` sequence stripped) alongside the extracted commands.
    fn split(chunks: &[&[u8]]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let mut tok = ApcTokenizer::new();
        let mut pass = Vec::new();
        let mut cmds = Vec::new();
        for c in chunks {
            tok.feed(
                c,
                |b| pass.extend_from_slice(b),
                |cmd| cmds.push(cmd.to_vec()),
            );
        }
        (pass, cmds)
    }

    #[test]
    fn extracts_a_graphics_command_between_esc_underscore_and_st() {
        assert_eq!(
            collect(&[b"\x1b_Ga=T,f=32;AAAA\x1b\\"]),
            vec![b"Ga=T,f=32;AAAA".to_vec()]
        );
    }

    #[test]
    fn non_graphics_apc_is_ignored() {
        // An APC that doesn't start with `G` (e.g. some other program's APC) is
        // discarded, and a graphics command right after is still caught.
        assert_eq!(
            collect(&[b"\x1b_Xhello\x1b\\\x1b_Ga=q;AAAA\x1b\\"]),
            vec![b"Ga=q;AAAA".to_vec()]
        );
    }

    #[test]
    fn command_split_across_reads_is_reassembled() {
        assert_eq!(
            collect(&[b"\x1b_Ga=T,", b"f=32;AA", b"AA\x1b", b"\\"]),
            vec![b"Ga=T,f=32;AAAA".to_vec()]
        );
    }

    #[test]
    fn byte_at_a_time_delivery_crosses_every_state() {
        let stream = b"plain\x1b_Ga=q;AAAA\x1b\\more";
        let chunks: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(collect(&chunks), vec![b"Ga=q;AAAA".to_vec()]);
    }

    #[test]
    fn passthrough_is_input_with_graphics_stripped() {
        let (pass, cmds) = split(&[b"before\x1b_Ga=T;AAAA\x1b\\after"]);
        assert_eq!(pass, b"beforeafter".to_vec());
        assert_eq!(cmds, vec![b"Ga=T;AAAA".to_vec()]);
    }

    #[test]
    fn non_graphics_apc_passes_through_verbatim() {
        // A foreign APC (e.g. tmux) must reach the client's VT parser unchanged,
        // while a graphics command in the same stream is still stripped.
        let (pass, cmds) = split(&[b"\x1b_Xhello\x1b\\\x1b_Ga=q;AAAA\x1b\\end"]);
        assert_eq!(pass, b"\x1b_Xhello\x1b\\end".to_vec());
        assert_eq!(cmds, vec![b"Ga=q;AAAA".to_vec()]);
    }

    #[test]
    fn lone_esc_and_other_escapes_are_preserved_in_passthrough() {
        // A CSI sequence and a bare ESC must survive; only `_G` is removed.
        let (pass, cmds) = split(&[b"a\x1b[31mred\x1b_Ga=d\x1b\\z"]);
        assert_eq!(pass, b"a\x1b[31mredz".to_vec());
        assert_eq!(cmds, vec![b"Ga=d".to_vec()]);
    }

    #[test]
    fn passthrough_survives_being_split_mid_sequence() {
        // Feed the same stream one byte at a time; the passthrough must still be
        // exactly the input minus the graphics command.
        let stream = b"x\x1b_Ga=T;QUJD\x1b\\y\x1b_Zother\x1b\\z";
        let chunks: Vec<&[u8]> = stream.chunks(1).collect();
        let (pass, cmds) = split(&chunks);
        assert_eq!(pass, b"xy\x1b_Zother\x1b\\z".to_vec());
        assert_eq!(cmds, vec![b"Ga=T;QUJD".to_vec()]);
    }

    #[test]
    fn sniffer_ties_tokenizer_and_parser_together() {
        let mut s = GraphicsSniffer::new();
        let mut pass = Vec::new();
        let events = s.feed(b"hi\x1b_Gi=1,a=q,t=d;AAAA\x1b\\bye", |b| {
            pass.extend_from_slice(b)
        });
        assert_eq!(pass, b"hibye".to_vec());
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::Query { reply, honored } => {
                assert!(honored);
                assert_eq!(reply, b"\x1b_Gi=1;OK\x1b\\");
            }
            _ => panic!("expected query"),
        }
    }

    #[test]
    fn sniff_takes_zero_copy_fast_path_without_graphics() {
        let mut s = GraphicsSniffer::new();
        // A chunk full of CSI escapes but no APC borrows straight through.
        match s.sniff(b"\x1b[31mred\x1b[0m plain") {
            Sniffed::Plain(b) => assert_eq!(b, b"\x1b[31mred\x1b[0m plain"),
            Sniffed::Segments(_) => panic!("expected the borrowed fast path"),
        }
    }

    #[test]
    fn sniff_preserves_stream_order_of_text_and_images() {
        let pixel = [0xffu8, 0x00, 0x00, 0xff];
        let b64 = BASE64.encode(pixel);
        let stream = format!("A\x1b_Ga=T,f=32,t=d,s=1,v=1,i=1;{b64}\x1b\\B");
        let mut s = GraphicsSniffer::new();
        let segs = match s.sniff(stream.as_bytes()) {
            Sniffed::Segments(segs) => segs,
            Sniffed::Plain(_) => panic!("graphics present"),
        };
        // Text "A", then the image, then text "B" — in that order.
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0], Segment::Output(b"A".to_vec()));
        assert!(matches!(&segs[1], Segment::Image(img) if img.id == 1));
        assert_eq!(segs[2], Segment::Output(b"B".to_vec()));
    }

    #[test]
    fn sniff_emits_query_and_delete_segments() {
        let mut s = GraphicsSniffer::new();
        let segs = match s.sniff(b"\x1b_Gi=7,a=q,t=d;AAAA\x1b\\x\x1b_Ga=d,d=A\x1b\\") {
            Sniffed::Segments(segs) => segs,
            Sniffed::Plain(_) => panic!("graphics present"),
        };
        assert_eq!(segs[0], Segment::Query(b"\x1b_Gi=7;OK\x1b\\".to_vec()));
        assert_eq!(segs[1], Segment::Output(b"x".to_vec()));
        assert!(matches!(&segs[2], Segment::Delete(d) if d.target == b'A'));
    }

    #[test]
    fn sniff_coalesces_adjacent_passthrough_runs() {
        // A foreign APC between two text runs is passthrough, so the whole thing
        // collapses to a single output segment (no graphics segment at all).
        let mut s = GraphicsSniffer::new();
        match s.sniff(b"a\x1b_Zother\x1b\\b") {
            Sniffed::Segments(segs) => {
                assert_eq!(segs, vec![Segment::Output(b"a\x1b_Zother\x1b\\b".to_vec())]);
            }
            Sniffed::Plain(_) => panic!("an APC opener forces the slow path"),
        }
    }

    #[test]
    fn resyncs_on_new_apc_after_an_unterminated_one() {
        assert_eq!(
            collect(&[b"\x1b_Ga=T;dropped\x1b_Ga=q;AAAA\x1b\\"]),
            vec![b"Ga=q;AAAA".to_vec()]
        );
    }

    #[test]
    fn oversized_chunk_is_abandoned_and_stream_recovers() {
        let mut big = b"\x1b_G".to_vec();
        big.extend(std::iter::repeat_n(b'x', MAX_APC_PAYLOAD + 1));
        big.extend_from_slice(b"\x1b\\\x1b_Ga=q;AAAA\x1b\\");
        assert_eq!(collect(&[&big]), vec![b"Ga=q;AAAA".to_vec()]);
    }

    #[test]
    fn control_parses_the_keys_terminal_browser_sends() {
        let c =
            Control::parse(b"Ga=T,f=32,o=z,s=1920,v=1080,t=d,i=42,p=1,C=1,q=2,m=1;xxxx").unwrap();
        assert_eq!(c.action, Action::TransmitAndDisplay);
        assert_eq!(c.format, WireFormat::Rgba);
        assert!(c.compressed);
        assert_eq!(c.width, 1920);
        assert_eq!(c.height, 1080);
        assert_eq!(c.medium, Medium::Direct);
        assert_eq!(c.id, 42);
        assert!(c.more);
        assert_eq!(c.quiet, 2);
    }

    #[test]
    fn query_reply_refuses_shm_file_on_remote_pane() {
        // The default parser is non-local (the SSH-safe posture): direct is
        // honored, file/shm are refused so the sender falls back to inline.
        let mut p = GraphicsParser::new();
        // The exact probe terminal-browser's graphics.ts sends.
        let ev = p.feed(b"Gi=4207,a=q,t=d,f=24,s=1,v=1;AAAA").unwrap();
        match ev {
            Event::Query { reply, honored } => {
                assert!(honored);
                assert_eq!(reply, b"\x1b_Gi=4207;OK\x1b\\".to_vec());
            }
            _ => panic!("expected query"),
        }
        // The shm/file medium probes must be refused so the sender falls back.
        let ev = p.feed(b"Gi=299,a=q,t=s,f=32,s=1,v=1;L3B4LXE").unwrap();
        match ev {
            Event::Query { reply, honored } => {
                assert!(!honored);
                assert_eq!(reply, b"\x1b_Gi=299;ENOTSUPPORTED\x1b\\".to_vec());
            }
            _ => panic!("expected query"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn query_reply_ok_for_shm_file_on_local_pane() {
        // A local pane shares the sender's filesystem, so file/shm are honored:
        // this is what unlocks terminal-browser's zero-inflate fast path.
        let mut p = GraphicsParser::new_local(true);
        for probe in [
            &b"Gi=299,a=q,t=s,f=32,s=1,v=1;L3B4LXE"[..],
            &b"Gi=300,a=q,t=f,f=32,s=1,v=1;L3RtcC94"[..],
            &b"Gi=301,a=q,t=t,f=32,s=1,v=1;L3RtcC94"[..],
        ] {
            match p.feed(probe).unwrap() {
                Event::Query { honored, reply } => {
                    assert!(honored, "local pane should honor {probe:?}");
                    assert!(reply.ends_with(b";OK\x1b\\"), "reply was {reply:?}");
                }
                _ => panic!("expected query"),
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn shared_transmission_surfaces_medium_transfer_on_local() {
        // `terminal-browser`'s shm transmit template: raw f=32, no o=z, the
        // shm object name base64'd after the `;`.
        let name = b"/px-abc123";
        let b64 = BASE64.encode(name);
        let cmd = format!("Ga=T,f=32,t=s,s=64,v=1,i=7,S=256;{b64}");
        let mut p = GraphicsParser::new_local(true);
        match p.feed(cmd.as_bytes()).unwrap() {
            Event::ImageFromMedium(t) => {
                assert_eq!(t.medium, Medium::Shared);
                assert_eq!(t.name, name);
                assert_eq!(t.id, 7);
                assert_eq!((t.width, t.height), (64, 1));
                assert_eq!(t.size, 256);
                assert!(!t.compressed);
            }
            _ => panic!("expected medium transfer"),
        }
    }

    #[test]
    fn shared_transmission_dropped_on_remote() {
        // A non-local parser must never surface a file/shm transfer even if a
        // misbehaving sender ignored the refusal and sent one anyway.
        let b64 = BASE64.encode(b"/px-abc123");
        let cmd = format!("Ga=T,f=32,t=s,s=64,v=1,i=7;{b64}");
        let mut p = GraphicsParser::new();
        assert_eq!(p.feed(cmd.as_bytes()), None);
    }

    #[test]
    #[cfg(unix)]
    fn file_transfer_resolves_and_temp_file_is_removed() {
        // A temp-file transfer: write raw RGBA to a temp path, resolve it, and
        // confirm the file is deleted afterward (the one-shot handoff contract).
        let rgba = [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tty7-kitty-test-{}.rgba", std::process::id()));
        std::fs::write(&path, rgba).unwrap();
        let t = MediumTransfer {
            medium: Medium::TempFile,
            name: path.clone().into_os_string().into_encoded_bytes(),
            offset: 0,
            size: 0,
            id: 1,
            number: 0,
            placement: 0,
            width: 2,
            height: 1,
            cols: 0,
            rows: 0,
            format: WireFormat::Rgba,
            compressed: false,
        };
        let img = t.resolve().expect("resolve temp file");
        assert_eq!(img.data, rgba);
        assert_eq!(img.to_rgba8().unwrap(), rgba);
        assert!(!path.exists(), "temp file should be removed after read");
    }

    /// Build a `t=t` transfer naming `path`, the shape a hostile escape uses.
    #[cfg(unix)]
    fn temp_file_transfer(path: &std::path::Path) -> MediumTransfer {
        MediumTransfer {
            medium: Medium::TempFile,
            name: path.to_path_buf().into_os_string().into_encoded_bytes(),
            offset: 0,
            size: 0,
            id: 1,
            number: 0,
            placement: 0,
            width: 2,
            height: 1,
            cols: 0,
            rows: 0,
            format: WireFormat::Rgba,
            compressed: false,
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_temp_file_transfer_outside_the_temp_dir_is_read_but_not_deleted() {
        // `t=t` names an arbitrary path out of an escape sequence — `cat`ing a
        // hostile file is enough to send one. Unlinking whatever it points at
        // would delete e.g. `~/.ssh/id_ed25519`. Read it, leave it.
        //
        // `/etc/hosts` stands in for the victim: a real, readable file that is
        // never under a temp dir on any host we build for. Deliberately *not*
        // something derived from `CARGO_MANIFEST_DIR` — a checkout can itself
        // live under `/tmp`, which would make the test assert the opposite of
        // what it means to.
        let victim = std::path::Path::new("/etc/hosts");
        let img = temp_file_transfer(victim).resolve().expect("still reads");
        assert!(!img.data.is_empty());
        assert!(
            victim.exists(),
            "a path outside the temp dir must survive a t=t handoff"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_temp_dir_check_resolves_symlinks_and_dotdot_before_comparing() {
        // The prefix has to be checked on the *canonicalized* path: `/tmp/../etc`
        // starts with `/tmp` textually but lands nowhere near it. macOS also
        // routes both `/tmp` and `TMPDIR` through `/private`, so a raw string
        // compare would reject the legitimate case too.
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("frame.rgba");
        std::fs::write(&inside, [0u8; 4]).unwrap();
        assert!(path_is_in_temp_dir(&inside));

        assert!(!path_is_in_temp_dir(std::path::Path::new(
            "/tmp/../etc/hosts"
        )));
        assert!(!path_is_in_temp_dir(std::path::Path::new("/etc/hosts")));
        // A path that doesn't resolve at all can't be vouched for.
        assert!(!path_is_in_temp_dir(std::path::Path::new(
            "/nonexistent-tty7-test-path"
        )));
        // The temp dir itself is not a file we'd ever unlink.
        assert!(!path_is_in_temp_dir(&std::env::temp_dir()));
    }

    #[test]
    #[cfg(unix)]
    fn a_transfer_naming_a_character_device_is_refused() {
        // `fs::read` on `/dev/zero` never returns, and this runs on the daemon's
        // reader thread — the pane's whole output path would wedge.
        let t = temp_file_transfer(std::path::Path::new("/dev/zero"));
        assert_eq!(t.resolve(), None);
    }

    #[test]
    #[cfg(unix)]
    fn a_malformed_shm_name_is_refused_before_shm_open() {
        for name in [
            &b"/../../etc/passwd"[..],
            &b"/sub/dir"[..],
            &b"no-leading-slash"[..],
            &b"/"[..],
            &b"/.."[..],
        ] {
            assert!(
                !shm_name_is_wellformed(name),
                "{:?} should be refused",
                String::from_utf8_lossy(name)
            );
        }
        assert!(shm_name_is_wellformed(b"/px-abc123"));
    }

    #[test]
    fn a_bomb_that_declares_huge_dimensions_is_still_bounded() {
        // The declared size is the sender's to choose, so bounding the inflate
        // by it alone isn't enough: `s=65535,v=65535` works out to a 17 GB
        // budget, which hands a bomb back exactly the allocation the bound was
        // meant to deny. The absolute `MAX_IMAGE_BYTES` clamp is what closes it.
        // (`to_rgba8_bounds_the_inflate_by_declared_dimensions` covers the
        // ordinary case, where the declared size is the tighter of the two.)
        let img = Image {
            id: 1,
            number: 0,
            placement: 0,
            width: 65535,
            height: 65535,
            cols: 0,
            rows: 0,
            // Has to inflate past `MAX_IMAGE_BYTES` to exercise the clamp at
            // all — anything smaller decodes on its merits and proves nothing.
            // Zeros deflate to a few KB, so the payload on the wire stays tiny,
            // which is the whole point of the attack.
            data: miniz_oxide::deflate::compress_to_vec_zlib(&vec![0u8; MAX_IMAGE_BYTES + 1], 6),
            format: WireFormat::Rgba,
            compressed: true,
        };
        assert!(
            img.decoded_len().unwrap() > MAX_IMAGE_BYTES,
            "the declared budget must be the looser bound for this to test anything"
        );
        assert!(
            img.data.len() < 1 << 20,
            "the compressed payload must stay small: {}",
            img.data.len()
        );
        assert_eq!(img.to_rgba8(), None, "the clamp has to reject it");
    }

    #[test]
    fn a_transmission_cap_leaves_room_for_the_wire_frame() {
        // `MAX_TRANSMISSION_BASE64` has to decode to something that still fits
        // in a `MAX_FRAME` wire frame with the header on it — otherwise
        // `write_frame` fails, and the daemon's writer treats that as fatal and
        // drops the client's whole connection over one image.
        let decoded_max = MAX_TRANSMISSION_BASE64 / 4 * 3;
        assert!(
            decoded_max + HEADER_LEN <= crate::daemon::protocol::MAX_FRAME,
            "{decoded_max} + {HEADER_LEN} must fit in {}",
            crate::daemon::protocol::MAX_FRAME
        );
        assert!(MAX_IMAGE_BYTES + HEADER_LEN <= crate::daemon::protocol::MAX_FRAME);
    }

    #[test]
    fn a_quiet_sender_gets_no_reply_written_back_to_its_pty() {
        // The reply is written to the *PTY* — it arrives as if typed. `q=1`
        // suppresses success, `q=2` suppresses failures too.
        let mut p = GraphicsParser::new_local(true);
        assert_eq!(p.feed(b"Gi=1,a=q,t=d,q=1;AAAA"), None);
        assert_eq!(p.feed(b"Gi=2,a=q,t=d,q=2;AAAA"), None);
        // A refusal still reaches a `q=1` sender, which needs to hear it to fall
        // back, and an unquiet probe is answered as before.
        let mut remote = GraphicsParser::new();
        assert!(matches!(
            remote.feed(b"Gi=3,a=q,t=s,q=1;AAAA"),
            Some(Event::Query { honored: false, .. })
        ));
        assert!(matches!(
            p.feed(b"Gi=4,a=q,t=d;AAAA"),
            Some(Event::Query { honored: true, .. })
        ));
    }

    #[test]
    fn a_quiet_key_past_a_byte_does_not_wrap_to_chatty() {
        assert_eq!(Control::parse(b"Ga=q,q=256;").unwrap().quiet, 255);
    }

    #[test]
    fn graphics_after_an_unterminated_foreign_apc_are_still_lifted() {
        // A foreign APC that never sends its ST used to swallow every `ESC _G`
        // after it: the graphics were forwarded as APC text, which the client's
        // vte then discards, so the image vanished with no trace.
        let input = b"\x1b_somevendor\x1b_Ga=q;AAAA\x1b\\";
        assert_eq!(collect(&[&input[..]]), vec![b"Ga=q;AAAA".to_vec()]);
    }

    #[test]
    fn an_escape_interrupting_a_graphics_command_still_reaches_the_client() {
        // The abandoned command's bytes stay stripped, but the escape that
        // interrupted it belongs to the terminal — swallowing it turned a
        // following `\x1b[31m` into literal `31m` on screen.
        let mut t = ApcTokenizer::new();
        let mut out = Vec::new();
        t.feed(b"\x1b_Gxx\x1b[31mred", |b| out.extend_from_slice(b), |_| {});
        assert_eq!(out, b"\x1b[31mred");
    }

    #[test]
    fn a_place_only_command_emits_nothing_rather_than_an_empty_image() {
        // `a=p` places an image transmitted earlier, which needs a stored-image
        // table this parser doesn't keep. It used to fall through the chunk
        // accumulator and emit an `Image` with no payload.
        let mut p = GraphicsParser::new_local(true);
        assert_eq!(p.feed(b"Ga=p,i=42,p=1;"), None);
    }

    #[test]
    #[cfg(unix)]
    fn an_oversized_size_key_truncates_instead_of_dropping_the_frame() {
        // kitty truncates an `S=` that runs past the object; discarding the
        // whole frame loses an image a real sender would have seen rendered.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame.rgba");
        std::fs::write(&path, [1u8, 2, 3, 4]).unwrap();
        let mut t = temp_file_transfer(&path);
        t.medium = Medium::File; // leave the file in place
        t.size = 4096;
        assert_eq!(t.resolve().expect("truncated, not dropped").data.len(), 4);
    }

    #[test]
    fn single_shot_rgba_transmission_decodes() {
        // One 1x1 opaque-red RGBA pixel, uncompressed, direct.
        let pixel = [0xffu8, 0x00, 0x00, 0xff];
        let b64 = BASE64.encode(pixel);
        let cmd = format!("Ga=T,f=32,t=d,s=1,v=1,i=7;{b64}");
        let mut p = GraphicsParser::new();
        let ev = p.feed(cmd.as_bytes()).unwrap();
        match ev {
            Event::Image(img) => {
                assert_eq!(img.id, 7);
                assert_eq!((img.width, img.height), (1, 1));
                assert!(!img.compressed);
                assert_eq!(img.to_rgba8().unwrap(), pixel);
                assert_eq!(img.format, WireFormat::Rgba);
            }
            _ => panic!("expected image"),
        }
    }

    #[test]
    fn chunked_compressed_transmission_reassembles_and_inflates() {
        // 2x1 RGBA (red, green), zlib-compressed, split into two direct chunks
        // exactly the way terminal-browser frames a `t=d,o=z` transmission.
        let rgba = [0xffu8, 0, 0, 0xff, 0x00, 0xff, 0x00, 0xff];
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&rgba, 1);
        let b64 = BASE64.encode(&z);
        let mid = b64.len() / 2;
        let first = format!("Ga=T,f=32,o=z,t=d,s=2,v=1,i=9,m=1;{}", &b64[..mid]);
        let second = format!("Gm=0;{}", &b64[mid..]);

        let mut p = GraphicsParser::new();
        assert_eq!(p.feed(first.as_bytes()), None); // more chunks pending
        let ev = p.feed(second.as_bytes()).unwrap();
        match ev {
            Event::Image(img) => {
                assert_eq!(img.id, 9);
                assert_eq!((img.width, img.height), (2, 1));
                assert!(img.compressed);
                // Wire payload stays compressed; the client inflates.
                assert_eq!(img.data, z);
                assert_eq!(img.to_rgba8().unwrap(), rgba);
            }
            _ => panic!("expected image"),
        }
    }

    #[test]
    fn rgb_transmission_is_expanded_to_opaque_rgba() {
        let rgb = [0x10u8, 0x20, 0x30]; // one pixel
        let b64 = BASE64.encode(rgb);
        let cmd = format!("Ga=T,f=24,t=d,s=1,v=1;{b64}");
        let mut p = GraphicsParser::new();
        match p.feed(cmd.as_bytes()).unwrap() {
            Event::Image(img) => {
                assert_eq!(img.to_rgba8().unwrap(), [0x10, 0x20, 0x30, 0xff])
            }
            _ => panic!("expected image"),
        }
    }

    #[test]
    fn delete_is_surfaced() {
        let mut p = GraphicsParser::new();
        match p.feed(b"Ga=d,d=A").unwrap() {
            Event::Delete(c) => {
                assert_eq!(c.action, Action::Delete);
                assert_eq!(c.delete, b'A');
            }
            _ => panic!("expected delete"),
        }
    }

    #[test]
    fn image_delete_normalizes_and_roundtrips() {
        // A bare `a=d` (no selector) means "all visible placements" (`a`).
        let bare = ImageDelete::from_control(&Control::parse(b"Ga=d").unwrap());
        assert_eq!(bare.target, b'a');
        // A scoped delete keeps its selector and id.
        let scoped = ImageDelete::from_control(&Control::parse(b"Ga=d,d=i,i=5").unwrap());
        assert_eq!((scoped.target, scoped.id), (b'i', 5));
        assert_eq!(ImageDelete::decode(&scoped.encode()), Some(scoped));
        assert_eq!(ImageDelete::decode(&[b'a', 0, 0]), None); // too short
    }

    #[test]
    fn image_frame_roundtrips_without_touching_the_payload() {
        let img = Image {
            id: 42,
            number: 3,
            placement: 1,
            width: 1920,
            height: 1080,
            cols: 80,
            rows: 24,
            data: vec![1, 2, 3, 4, 5, 6, 7],
            format: WireFormat::Rgba,
            compressed: true,
        };
        let frame = img.encode_frame();
        // Header + payload, payload byte-identical (no base64 inflation).
        assert_eq!(&frame[HEADER_LEN..], &img.data[..]);
        assert_eq!(Image::decode_frame(&frame), Some(img));
        // A truncated frame is rejected, not panicked on.
        assert_eq!(Image::decode_frame(&frame[..HEADER_LEN - 1]), None);
    }

    #[test]
    fn to_rgba8_bounds_the_inflate_by_declared_dimensions() {
        // A hostile payload: a tiny compressed blob that inflates far past the
        // 1x1 image it claims to be. Without a cap this would balloon into a
        // multi-MB (in the wild, multi-GB) allocation; bounded by the declared
        // `width * height * 4` it must be rejected instead.
        let bomb = vec![0u8; 4 * 1024 * 1024]; // 4 MiB of zeros → tiny deflate
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&bomb, 9);
        assert!(z.len() < bomb.len(), "payload must actually compress");
        let img = Image {
            id: 1,
            number: 0,
            placement: 0,
            width: 1,
            height: 1,
            cols: 0,
            rows: 0,
            data: z,
            format: WireFormat::Rgba,
            compressed: true,
        };
        assert_eq!(img.to_rgba8(), None, "an over-budget inflate is dropped");

        // A payload that inflates to exactly its declared size still decodes.
        let pixels = vec![0xabu8; 4]; // 1x1 RGBA
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&pixels, 9);
        let ok = Image { data: z, ..img };
        assert_eq!(ok.to_rgba8().as_deref(), Some(&pixels[..]));
    }
}

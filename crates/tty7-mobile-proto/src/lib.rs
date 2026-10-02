//! The wire protocol between the tty7 mobile app and the desktop gateway.
//!
//! The phone never speaks the daemon's own protocols. Those are private to one
//! machine — no auth beyond socket permissions, message kinds that can hand a
//! pane's process over or replace the server binary — and they change version
//! whenever the desktop grows a feature. The gateway (`tty7-gateway`) sits on
//! the desktop, speaks both, and exposes only this small vocabulary:
//!
//! - one iroh connection per phone, authenticated by the phone's endpoint key;
//! - one bidirectional QUIC stream per purpose, opened by the phone, whose first
//!   frame is an [`Open`] and whose first answer is an [`OpenReply`];
//! - a [`Open::Control`] stream carrying [`Tree`] snapshots of every
//!   workspace, tab, pane and agent status on the machine;
//! - a [`Open::Pane`] stream per terminal on screen: raw output down, raw
//!   keystrokes up, and a few [`PaneEvent`]s beside them;
//! - a one-shot [`Open::NewTab`] stream that starts a shell in a new tab and
//!   answers with a [`TabCreated`];
//! - a one-shot [`Open::Upload`] stream that carries a file from the phone to
//!   the machine and answers with where it landed, an [`Uploaded`];
//! - a one-shot [`Open::Diff`] stream that answers with a git working tree's
//!   changes, a [`Diff`].
//!
//! Every frame is `u32` little-endian length, a one-byte kind, then the
//! payload — the same shape as the daemon's frames, so there is one framing
//! rule across tty7. Kind [`KIND_JSON`] carries a JSON message; kind
//! [`KIND_BYTES`] carries terminal bytes verbatim (output downstream, input
//! upstream), which would triple in size as JSON.

use std::io;

use serde::{Deserialize, Serialize};

/// The ALPN both ends pass to iroh; a connection with any other is refused
/// during the handshake, before a byte of this protocol is read.
pub const ALPN: &[u8] = b"tty7/mobile/1";

/// The mDNS service both ends use to find each other on a local network,
/// whatever addresses the gateway has moved to since pairing.
pub const MDNS_SERVICE: &str = "tty7";

/// Bumped when a message changes shape. Carried in [`OpenReply::Ok`] so an app
/// can tell the user to update rather than misread a newer gateway.
pub const PROTOCOL_VERSION: u32 = 1;

/// The largest file an [`Open::Upload`] may carry: photos, screenshots and
/// small files, not a way to move a disk image.
pub const MAX_UPLOAD: u64 = 20 << 20;

/// The largest frame either side will accept. Output is forwarded in the chunks
/// the daemon sends, and a replay segment is capped well below this on the
/// daemon side; anything larger is a corrupt or hostile stream.
pub const MAX_FRAME: usize = 16 << 20;

pub const KIND_JSON: u8 = 0;
pub const KIND_BYTES: u8 = 1;

/// The first frame on every stream the phone opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Open {
    /// Trade a one-time pairing secret for a place on the gateway's device
    /// list. The only stream an unknown device may open.
    Pair { secret: String, device_name: String },
    /// Subscribe to the machine's [`Tree`].
    Control,
    /// Watch one pane and type into it.
    ///
    /// The gateway observes the pane rather than attaching to it: an attach
    /// would take the pane away from the window showing it. Keystrokes go in
    /// beside the observer, so the phone can drive the pane without owning
    /// it. The pane keeps the desktop's size unless the phone asks for its
    /// own with [`PaneRequest::TakeOver`].
    Pane {
        pane_id: u64,
        /// The [`RemoteView::key`] of the remote machine the pane lives on;
        /// absent for the gateway's own machine.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        machine: Option<String>,
    },
    /// Start a shell in a new tab at the end of a workspace. One-shot: the
    /// gateway creates the tab first, then answers `Ok` followed by a
    /// [`TabCreated`], or `Denied` with what went wrong.
    ///
    /// A gateway older than this variant cannot parse it and drops the stream
    /// unanswered.
    NewTab {
        workspace_id: String,
        /// Where the shell starts; the server's default (home) when absent.
        #[serde(default)]
        cwd: Option<String>,
        /// The grid the shell starts at — the phone's screen, since no desktop
        /// window is showing the tab yet. One that later does resizes it.
        #[serde(default)]
        size: Option<GridSize>,
        /// As on [`Open::Pane`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        machine: Option<String>,
    },
    /// Close a tab, its panes with it. One-shot: `Ok` once it is closed, or
    /// `Denied` with why not. Where the machine keeps closed tabs, it goes on
    /// the workspace's recently-closed list, for the desktop to reopen.
    ///
    /// A gateway older than this variant cannot parse it and drops the stream
    /// unanswered.
    CloseTab {
        workspace_id: String,
        tab_id: String,
        /// As on [`Open::Pane`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        machine: Option<String>,
    },
    /// What has changed in the git working tree `cwd` is in, against its last
    /// commit: `Ok`, then a [`Diff`]. `Denied` when it is not in a repository.
    ///
    /// A gateway older than this variant cannot parse it and drops the stream
    /// unanswered.
    Diff {
        cwd: String,
        /// As on [`Open::Pane`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        machine: Option<String>,
    },
    /// Put a file from the phone on the machine, for a pane to be handed its
    /// path. After `Ok` the phone sends the file as bytes frames and finishes
    /// its side; the gateway answers [`Uploaded`] once the file is written.
    /// `Denied` before any bytes: too big, or a machine files cannot go to.
    ///
    /// A gateway older than this variant cannot parse it and drops the stream
    /// unanswered.
    Upload {
        /// The file's name on the phone; the gateway keeps what it safely can.
        name: String,
        /// Its length in bytes, at most [`MAX_UPLOAD`].
        size: u64,
        /// As on [`Open::Pane`]: where the pane it is for runs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        machine: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

/// The answer on a [`Open::NewTab`] stream, after its `Ok`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabCreated {
    pub tab_id: String,
    pub pane_id: u64,
}

/// The answer on a [`Open::Diff`] stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    /// The repository's top directory.
    pub root: String,
    /// `git diff HEAD`: staged and unstaged together, as a unified diff.
    pub patch: String,
    /// Files git does not track yet (and does not ignore), by path from `root`.
    #[serde(default)]
    pub untracked: Vec<String>,
    /// The patch was cut at [`MAX_DIFF`] bytes.
    #[serde(default)]
    pub truncated: bool,
}

/// The most of a patch a [`Diff`] carries: enough to review an agent's work
/// on a phone, not a vendored dependency's.
pub const MAX_DIFF: usize = 2 << 20;

/// The answer on a [`Open::Upload`] stream once the file is written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uploaded {
    /// Where the file is, absolute, on the machine the pane runs on.
    pub path: String,
}

/// The gateway's answer to an [`Open`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpenReply {
    Ok { host: String, version: u32 },
    Denied { reason: String },
}

/// Phone → gateway on a control stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlRequest {
    /// Send a fresh [`Tree`] now, even if nothing changed. The app sends it on
    /// resume from background, where it may have missed nothing or everything.
    Refresh,
}

/// Gateway → phone on a control stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlEvent {
    /// The whole machine, sent on open and again whenever it changes.
    Tree(Tree),
    /// The gateway lost the daemon; a `Tree` follows when it is back.
    Error { message: String },
}

/// Phone → gateway on a pane stream, as JSON frames beside the keystrokes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneRequest {
    /// Run the pane at the phone's size while this stream is open, or change
    /// the size it runs at. The desktop keeps its window, is told the phone
    /// has the pane, and can take it back; closing the stream gives it back.
    /// Answered with [`PaneEvent::Lease`].
    TakeOver { size: GridSize },
    /// Give the pane back to the desktop's size.
    Release,
}

/// Gateway → phone on a pane stream, beside the [`KIND_BYTES`] output frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneEvent {
    /// The pane's grid size. Output that follows was written for this size,
    /// so the terminal must resize before writing it. Arrives once per replay
    /// segment and again whenever the desktop resizes the pane.
    Size {
        cols: u16,
        rows: u16,
    },
    Cwd {
        path: String,
    },
    Agent {
        agent: Option<AgentView>,
    },
    Exited {
        code: Option<i32>,
    },
    Error {
        message: String,
    },
    /// Whether the pane runs at this phone's size. `held: false` after a
    /// take-over means it ended: released, taken back on the desktop, or taken
    /// over by another device. `refused` says why a take-over did not start.
    Lease {
        held: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<String>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Tree {
    pub host: String,
    pub workspaces: Vec<WorkspaceView>,
    /// The machines the desktop is linked to over SSH, each with its own
    /// workspaces. The gateway reaches them through those links and never
    /// dials one itself.
    #[serde(default)]
    pub remotes: Vec<RemoteView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteView {
    /// The link's key (`me@build-box:22`): what [`Open::Pane`] and
    /// [`Open::NewTab`] name the machine by.
    pub key: String,
    /// What to call it: the host, or the key when it has none.
    pub name: String,
    /// Whether the desktop's link to it is up. A down link lists no
    /// workspaces; the desktop has to reconnect it.
    pub connected: bool,
    /// Why its workspaces could not be read, when the link is up but the far
    /// end did not answer.
    #[serde(default)]
    pub error: Option<String>,
    /// The link is up and its first read has not come back yet: its
    /// workspaces are unknown, not absent.
    #[serde(default)]
    pub pending: bool,
    pub workspaces: Vec<WorkspaceView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceView {
    pub id: String,
    pub name: String,
    pub tabs: Vec<TabView>,
    /// The desktop sidebar's groups, in its order: pinned groups, then the
    /// ones it works out per repository or SSH host, then the tabs in none.
    /// Every tab is in exactly one. Empty from a gateway that predates
    /// groups, and the tabs are then one list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<GroupView>,
    /// The tab the desktop last had in front in this workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_tab: Option<String>,
}

/// One of the desktop sidebar's groups of tabs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupView {
    /// What its header reads. `None` for the tabs in no group when there is
    /// no other group to set them apart from, which the desktop draws without
    /// a header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A group the user pinned, rather than one worked out from the tabs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// Folded shut on the desktop.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub collapsed: bool,
    /// Its tabs' ids, in the workspace's order.
    pub tabs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabView {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub hibernated: bool,
    pub panes: Vec<PaneView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneView {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub agent: Option<AgentView>,
    /// Nothing is running in it: the machine's server restarted, and no
    /// window has opened the tab since to start it again. Absent from an
    /// older gateway, which cannot tell.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentView {
    /// The agent's CLI, lower-case (`claude`, `codex`, …).
    pub kind: String,
    pub status: AgentStatus,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Waiting,
    Done,
}

/// What the desktop shows as a QR code and the phone scans to pair.
///
/// Carries the gateway's endpoint id (its public key — the thing the phone will
/// trust from then on), the addresses it could be reached at when the code was
/// made, and a one-time secret that proves the scanner was in front of the
/// screen. The addresses are only a head start: after pairing the phone dials
/// by id and iroh's discovery finds the gateway wherever it has moved to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairCode {
    pub host_id: String,
    pub host_name: String,
    #[serde(default)]
    pub relay: Option<String>,
    #[serde(default)]
    pub addrs: Vec<String>,
    pub secret: String,
}

const PAIR_PREFIX: &str = "tty7pair:";

impl PairCode {
    pub fn encode(&self) -> String {
        use base64::Engine as _;
        let json = serde_json::to_vec(self).expect("PairCode serializes");
        format!(
            "{PAIR_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
        )
    }

    pub fn decode(code: &str) -> Result<PairCode, String> {
        use base64::Engine as _;
        let body = code
            .trim()
            .strip_prefix(PAIR_PREFIX)
            .ok_or_else(|| "not a tty7 pairing code".to_string())?;
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|e| format!("pairing code is damaged: {e}"))?;
        serde_json::from_slice(&json).map_err(|e| format!("pairing code is damaged: {e}"))
    }
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Json(Vec<u8>),
    Bytes(Vec<u8>),
}

impl Frame {
    /// Parses a [`Frame::Json`] as `T`. A bytes frame where a message was
    /// expected is a protocol error, not something to skip.
    pub fn msg<T: for<'de> Deserialize<'de>>(&self) -> io::Result<T> {
        match self {
            Frame::Json(json) => serde_json::from_slice(json).map_err(invalid),
            Frame::Bytes(_) => Err(invalid("expected a message, got a bytes frame")),
        }
    }
}

pub fn encode_msg<T: Serialize>(msg: &T) -> Vec<u8> {
    let json = serde_json::to_vec(msg).expect("protocol messages serialize");
    encode_frame(KIND_JSON, &json)
}

pub fn encode_bytes(bytes: &[u8]) -> Vec<u8> {
    encode_frame(KIND_BYTES, bytes)
}

fn encode_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len() + 1).expect("frame fits u32");
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.extend_from_slice(&len.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    out
}

/// Checks a frame header and returns the payload length that follows it.
fn payload_len(header: [u8; 5]) -> io::Result<(u8, usize)> {
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if len == 0 {
        return Err(invalid("frame with no kind byte"));
    }
    if len > MAX_FRAME {
        return Err(invalid(format!("frame of {len} bytes exceeds the limit")));
    }
    let kind = header[4];
    if kind != KIND_JSON && kind != KIND_BYTES {
        return Err(invalid(format!("unknown frame kind {kind}")));
    }
    Ok((kind, len - 1))
}

fn frame_of(kind: u8, payload: Vec<u8>) -> Frame {
    if kind == KIND_JSON {
        Frame::Json(payload)
    } else {
        Frame::Bytes(payload)
    }
}

/// Splits a byte stream into frames without owning the transport, for callers
/// that receive data in arbitrary chunks (a WebView bridge, a test).
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete frame, `Ok(None)` if more bytes are needed.
    pub fn next_frame(&mut self) -> io::Result<Option<Frame>> {
        let Some(header) = self.buf.first_chunk::<5>() else {
            return Ok(None);
        };
        let (kind, len) = payload_len(*header)?;
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let payload = self.buf[5..5 + len].to_vec();
        self.buf.drain(..5 + len);
        Ok(Some(frame_of(kind, payload)))
    }
}

#[cfg(feature = "tokio")]
mod io_async {
    use super::*;
    use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

    /// Reads one frame. `Ok(None)` is a clean end of stream between frames; an
    /// end in the middle of one is an error.
    pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Frame>> {
        let mut header = [0u8; 5];
        let mut got = 0;
        while got < header.len() {
            let n = r.read(&mut header[got..]).await?;
            if n == 0 {
                return if got == 0 {
                    Ok(None)
                } else {
                    Err(io::ErrorKind::UnexpectedEof.into())
                };
            }
            got += n;
        }
        let (kind, len) = payload_len(header)?;
        let mut payload = vec![0u8; len];
        r.read_exact(&mut payload).await?;
        Ok(Some(frame_of(kind, payload)))
    }

    pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(
        w: &mut W,
        msg: &T,
    ) -> io::Result<()> {
        w.write_all(&encode_msg(msg)).await
    }

    pub async fn write_bytes<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> io::Result<()> {
        w.write_all(&encode_bytes(bytes)).await
    }
}

#[cfg(feature = "tokio")]
pub use io_async::{read_frame, write_bytes, write_msg};

fn invalid(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_survive_arbitrary_chunking() {
        let mut wire = encode_msg(&Open::Pane {
            pane_id: 7,
            machine: None,
        });
        wire.extend(encode_bytes(b"\x1b[31mhi\r\n"));
        wire.extend(encode_msg(&PaneEvent::Size { cols: 80, rows: 24 }));

        for chunk in 1..wire.len() {
            let mut dec = Decoder::default();
            let mut frames = Vec::new();
            for piece in wire.chunks(chunk) {
                dec.push(piece);
                while let Some(f) = dec.next_frame().unwrap() {
                    frames.push(f);
                }
            }
            assert_eq!(frames.len(), 3, "chunk size {chunk}");
            assert_eq!(
                frames[0].msg::<Open>().unwrap(),
                Open::Pane {
                    pane_id: 7,
                    machine: None
                }
            );
            assert_eq!(frames[1], Frame::Bytes(b"\x1b[31mhi\r\n".to_vec()));
            assert_eq!(
                frames[2].msg::<PaneEvent>().unwrap(),
                PaneEvent::Size { cols: 80, rows: 24 }
            );
        }
    }

    #[test]
    fn oversized_and_unknown_frames_are_refused() {
        let mut dec = Decoder::default();
        dec.push(&(MAX_FRAME as u32 + 1).to_le_bytes());
        dec.push(&[KIND_BYTES]);
        assert!(dec.next_frame().is_err());

        let mut dec = Decoder::default();
        dec.push(&2u32.to_le_bytes());
        dec.push(&[9, 0]);
        assert!(dec.next_frame().is_err());

        let mut dec = Decoder::default();
        dec.push(&0u32.to_le_bytes());
        dec.push(&[0]);
        assert!(dec.next_frame().is_err());
    }

    #[test]
    fn a_bytes_frame_is_not_a_message() {
        assert!(
            Frame::Bytes(b"{}".to_vec())
                .msg::<ControlRequest>()
                .is_err()
        );
    }

    #[test]
    fn pair_code_round_trips_and_rejects_strangers() {
        let code = PairCode {
            host_id: "abc".into(),
            host_name: "studio".into(),
            relay: Some("https://relay.example/".into()),
            addrs: vec!["192.168.1.4:5000".into()],
            secret: "s3cret".into(),
        };
        let text = code.encode();
        assert!(text.starts_with("tty7pair:"));
        assert_eq!(PairCode::decode(&format!("  {text}\n")).unwrap(), code);
        assert!(PairCode::decode("https://example.com").is_err());
        assert!(PairCode::decode("tty7pair:!!!").is_err());
    }

    #[test]
    fn message_tags_are_stable() {
        // The app's TypeScript matches on these strings.
        let json = serde_json::to_value(PaneEvent::Exited { code: Some(1) }).unwrap();
        assert_eq!(json, serde_json::json!({"type": "exited", "code": 1}));
        let json = serde_json::to_value(AgentStatus::Waiting).unwrap();
        assert_eq!(json, serde_json::json!("waiting"));
    }

    #[test]
    fn take_over_tags_are_the_ones_the_app_matches() {
        let ask = serde_json::to_value(PaneRequest::TakeOver {
            size: GridSize { cols: 50, rows: 30 },
        })
        .unwrap();
        assert_eq!(
            ask,
            serde_json::json!({"type": "take_over", "size": {"cols": 50, "rows": 30}})
        );
        let held = serde_json::to_value(PaneEvent::Lease {
            held: true,
            refused: None,
        })
        .unwrap();
        assert_eq!(held, serde_json::json!({"type": "lease", "held": true}));
    }

    #[test]
    fn a_new_tab_asks_with_only_a_workspace() {
        let open: Open =
            serde_json::from_value(serde_json::json!({"type": "new_tab", "workspace_id": "w"}))
                .unwrap();
        assert_eq!(
            open,
            Open::NewTab {
                workspace_id: "w".into(),
                cwd: None,
                size: None,
                machine: None
            }
        );
        // A local pane is asked for exactly as before remotes existed, so an
        // older gateway still understands it.
        assert_eq!(
            serde_json::to_value(Open::Pane {
                pane_id: 3,
                machine: None
            })
            .unwrap(),
            serde_json::json!({"type": "pane", "pane_id": 3})
        );
    }

    #[test]
    fn an_upload_names_its_file_and_size() {
        let open = Open::Upload {
            name: "shot.png".into(),
            size: 42,
            machine: None,
        };
        let wire = serde_json::to_value(&open).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({"type": "upload", "name": "shot.png", "size": 42})
        );
        assert_eq!(serde_json::from_value::<Open>(wire).unwrap(), open);
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn async_reader_matches_the_decoder() {
        let mut wire = encode_msg(&ControlRequest::Refresh);
        wire.extend(encode_bytes(b"x"));
        let mut r = &wire[..];
        assert_eq!(
            read_frame(&mut r)
                .await
                .unwrap()
                .unwrap()
                .msg::<ControlRequest>()
                .unwrap(),
            ControlRequest::Refresh
        );
        assert_eq!(
            read_frame(&mut r).await.unwrap(),
            Some(Frame::Bytes(b"x".to_vec()))
        );
        assert_eq!(read_frame(&mut r).await.unwrap(), None);

        let truncated = &wire[..3];
        assert!(read_frame(&mut &truncated[..]).await.is_err());
    }
}

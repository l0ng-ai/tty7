//! The **control** dialect: a multiplexed request/response channel for
//! filesystem + git operations against a machine that isn't this one.
//!
//! The pane protocol in [`super::protocol`] is deliberately unmultiplexed — one
//! connection carries one PTY, requests have no ids, and a control request is a
//! short-lived connection of its own. That shape stops working the moment the
//! peer is on the other side of an ocean: a file tree expanding a directory
//! must not queue behind a `git status` that takes two seconds. So control gets
//! its own dialect on the same framing, with request ids and out-of-order
//! replies.
//!
//! ## Relationship to [`super::protocol`]
//!
//! | Shared | Separate |
//! |---|---|
//! | Outer frame `[u32 LE payload_len][u8 kind][payload]` | Kind space (control owns **60-63**) |
//! | [`MAX_FRAME`] (64 MiB) | Message enums, payload layout |
//! | `write_frame` / `read_frame` / `take_frame` | Request ids, timeouts, cancellation |
//!
//! Control kinds live in this module's own [`kind`], not `protocol`'s (which is
//! private by design). The two spaces never mix on one connection in a way that
//! could be ambiguous: a peer that speaks control announces it through
//! [`crate::daemon::protocol::PROTOCOL_VERSION`] ≥ 3 plus the
//! [`feature::CONTROL`] capability bit, and 60-63 sit clear of every range the
//! pane protocol has reserved (WS3 auth 15-19, WS4 forwards 20-24, SFTP 30-36)
//! and clear of the **retired** kind 13 (once `SPAWN_MANAGED_SSH`), which is
//! never reused — an old daemon would decode it as a pane spawn and silently do
//! the wrong thing rather than reporting an unknown kind.
//!
//! ## Payload layouts
//!
//! ```text
//! HELLO / HELLO_OK (60)
//! ┌──────────────────────────┐
//! │ JSON (payload_len bytes) │
//! └──────────────────────────┘
//!
//! REQUEST / RESPONSE (61), CANCEL (63, C→S), EVENT (63, S→C)
//! ┌───────────────┬───────────────┬──────────────────┐
//! │ u64 LE req_id │ u32 LE json_n │ JSON (json_n B)  │
//! └───────────────┴───────────────┴──────────────────┘
//! payload_len == 12 + json_n
//!
//! REQUEST_BLOB / RESPONSE_BLOB (62)
//! ┌───────────────┬───────────────┬─────────────────┬─────────────────────┐
//! │ u64 LE req_id │ u32 LE json_n │ JSON (json_n B) │ raw blob (the rest) │
//! └───────────────┴───────────────┴─────────────────┴─────────────────────┘
//! blob_len == payload_len - 12 - json_n
//! ```
//!
//! Every non-`HELLO` frame carries a `req_id` even when it can't have one
//! (events are always 0). The eight redundant bytes buy a single header parser:
//! read `u64`, read `u32`, take the JSON, *then* branch on kind.
//!
//! Bulk payloads keep a JSON head rather than being bare bytes because the
//! bytes alone can't carry their own parameters — `WriteFile` needs the target
//! path beside the content, and `ReadFile`'s reply needs the [`Meta`] the
//! editor would otherwise have to fetch in a second round trip.
//!
//! ## Request ids
//!
//! | | |
//! |---|---|
//! | Allocated by | the client, only; the server never mints one |
//! | Range | starts at 1, `fetch_add(1)`; **0 is reserved for server pushes** |
//! | Matching | out of order — a reply is claimed by id, not by arrival order |
//! | Unknown id in a reply | dropped silently (a timed-out request's reply may still land) |
//! | Shape | strictly one reply per request; no streaming, no continuation frames |

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::protocol::{MAX_FRAME, read_frame, write_frame};

/// The control dialect's own version, negotiated in [`ControlHello`] and
/// independent of [`crate::daemon::protocol::PROTOCOL_VERSION`]: the pane
/// protocol and the control dialect evolve on separate clocks, and a remote
/// `tty7-server` speaks control without necessarily serving panes at all.
///
/// Bump this when a new [`ControlRequest`] / [`ReplyOk`] variant lands. That is
/// stricter than the pane protocol's rule, and deliberately so: these enums have
/// no `#[serde(other)]` fallback either, but the consequence here is worse —
/// [`crate::daemon::install`] decides whether to *upgrade the remote binary* by
/// comparing dialect numbers, so a capability that doesn't move the number is a
/// capability the far machine never gets. A [`feature`] string is the right
/// answer only for something two current servers can genuinely disagree about
/// (the machine tree, which depends on how the server was started); "this
/// build knows the request and older ones don't" is what the number is for.
///
/// ## History
///
/// - **v3** — the machine-tree migration. The workspace/tab/pane tree moved
///   into the daemon: `MachineGet` / `WorkspaceTree`, the semantic tree verbs
///   (workspace/tab/pane create, close, rename, move, split, ratio, replace),
///   and the [`ControlEvent::Layout`] / [`ControlEvent::LayoutResync`] pushes
///   — seventeen new request variants in all — while the retired opaque-record
///   verbs
///   (`workspace_list` / `workspace_get` / `workspace_put` /
///   `workspace_delete` and the `workspace_changed` event) left the dialect
///   entirely. A v2 peer meeting any of the new variants fails the whole
///   decode (no `#[serde(other)]`), and this build meeting a v2 server's
///   record verbs would answer unknown-variant errors forever. The
///   [`feature::MACHINE_TREE`] bit still exists *within* v3, because two
///   current servers can genuinely differ on it (a box with no home
///   directory serves files but no tree).
/// - **v2** — [`ControlRequest::Shells`], which backs a remote window's new-tab
///   dropdown. Not a `feature` string: every server from this build on answers
///   it, so the only thing a capability bit would have bought is that a machine
///   running an older server keeps running it forever, silently serving an empty
///   menu. The bump makes `RemoteProtocol::serves` refuse to adopt that server
///   and install this build's instead, which is the actual fix.
/// - **v1** — the dialect at the time remote workspaces landed.
pub const CONTROL_VERSION: u32 = 3;

/// The phrase every control-dialect refusal contains.
///
/// Written down once so the message and the test that recognises it cannot
/// drift apart. See [`is_dialect_refusal`].
const DIALECT_MARKER: &str = "speaks control v";

/// The message a handshake between two incompatible control dialects fails
/// with.
fn dialect_refusal(peer_build: &str, peer: u32, ours: u32) -> String {
    format!("control peer (build {peer_build}) {DIALECT_MARKER}{peer}, this build speaks v{ours}")
}

/// Whether an error message is a control-dialect refusal rather than one of the
/// dozen other reasons a connect fails (a dead host, a bad key, a refused
/// port).
///
/// A marker in the text rather than a typed error because this message crosses
/// two process boundaries — the daemon's route ack, then the GUI's error card —
/// as a `String`, and rebuilding a type on the far side of that would be more
/// machinery than the one question needs.
///
/// The question is worth asking because the answer is unusually specific: a
/// dialect refusal is the *only* connect failure that a reinstall on the far end
/// fixes, and the only one where offering that (destructive) action is
/// justified. Everything else it must not offer it for.
pub fn is_dialect_refusal(message: &str) -> bool {
    message.contains(DIALECT_MARKER)
}

/// This process's identity as a control server, minted once on first use.
///
/// Answers "am I still talking to the same server?" — the question no other
/// field in [`ControlHelloOk`] can answer, because `build` and both version
/// numbers survive a restart unchanged (the remote's binary is replaced in
/// place, keeping its name). A client that reconnects and sees a different value
/// here *knows* every `pane_id` it holds names a pane in a process that no
/// longer exists.
///
/// Per **process**, not per connection: a server serves many connections and
/// they must all report the same instance, or the client would read every new
/// connection as a restart.
pub fn server_instance() -> &'static str {
    static INSTANCE: OnceLock<String> = OnceLock::new();
    INSTANCE.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

/// Paths coalesced into one [`ControlEvent::Watch`] window before the server
/// gives up on precision and sends [`ControlEvent::WatchOverflow`] instead,
/// which the client answers by invalidating the whole tree. A cap rather than
/// an unbounded batch because `cargo build` in a watched root can touch tens of
/// thousands of paths in a single 100 ms window, and re-listing beats shipping
/// them.
pub const WATCH_BURST_CAP: usize = 1024;

/// Rolling window the server coalesces filesystem events into. The local
/// implementation uses the same figure on purpose — a watcher that is "helpfully"
/// more responsive locally makes every timing-sensitive behavior diverge between
/// a local and a remote workspace.
pub const WATCH_COALESCE_WINDOW: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Kinds
// ---------------------------------------------------------------------------

/// Control frame kind bytes.
///
/// Client→server and server→client are independent spaces (a connection always
/// knows which direction it is reading), so the numeric overlap between, say,
/// [`kind::REQUEST`] and [`kind::RESPONSE`] is deliberate and mirrors how
/// `protocol`'s two spaces already work.
///
/// 64-69 are held open for later control frames (streaming replies,
/// backpressure signals) so the dialect never has to claim a second range.
pub mod kind {
    // ----- client -> server -------------------------------------------------

    /// Handshake. The only control frame without a `req_id`.
    pub const HELLO: u8 = 60;
    /// A request whose parameters fit in JSON.
    pub const REQUEST: u8 = 61;
    /// A request with bulk bytes trailing the JSON (`WriteFile`).
    pub const REQUEST_BLOB: u8 = 62;
    /// Abandon a request. Best-effort on the server; the client has already
    /// given up by the time it sends this.
    pub const CANCEL: u8 = 63;

    // ----- server -> client -------------------------------------------------

    /// Handshake reply. Sent even on a version mismatch (then the server hangs
    /// up), so the client can report *which* version it met.
    pub const HELLO_OK: u8 = 60;
    /// A reply whose payload fits in JSON.
    pub const RESPONSE: u8 = 61;
    /// A reply with bulk bytes trailing the JSON (`ReadFile`).
    pub const RESPONSE_BLOB: u8 = 62;
    /// An unsolicited push. `req_id` is always 0.
    pub const EVENT: u8 = 63;
}

/// Target size of a [`ControlEvent::GitChunk`] batch. Big enough that a
/// multi-megabyte diff is a few hundred frames rather than a frame per line,
/// small enough that neither side ever holds much of it.
pub const GIT_STREAM_CHUNK: usize = 64 * 1024;

/// Hard ceiling on one [`ControlEvent::GitChunk`]'s payload, as opposed to
/// [`GIT_STREAM_CHUNK`]'s target.
///
/// A batch is flushed once it *reaches* the target, so its size is the target
/// plus whatever the line that crossed it was. Without a ceiling such a batch
/// could encode to a frame larger than [`crate::daemon::protocol::MAX_FRAME`]
/// and not be sendable at all. Half the frame limit leaves room for base64's
/// four-thirds expansion and the JSON envelope around it. Splitting at this
/// size can land mid-line; that is safe precisely because the receiver
/// reassembles with [`crate::core::git::LineSplitter`], which spans chunk
/// boundaries.
///
/// A backstop rather than a working limit: [`crate::core::git::MAX_LINE`] caps
/// the line that crosses the target, so a real batch is a megabyte at worst and
/// never reaches this. It stays because the two bounds answer to different
/// things — one to what a diff viewer can show, this one to what a frame can
/// hold — and a batch that cannot be sent is a stream that never ends.
pub const GIT_STREAM_CHUNK_MAX: usize = crate::daemon::protocol::MAX_FRAME / 2;

/// How long a [`ControlRequest::GitStream`] reader waits for the *next* chunk
/// before giving up on the stream.
///
/// Idle time, deliberately, not elapsed time: a `git diff` over a big work tree
/// on a slow link is allowed to take as long as it takes, and any cap on the
/// total would eventually truncate a legitimate read. What it catches is the
/// case nothing else can — a link that is *alive* while the server's git is
/// wedged (a network filesystem that stops answering, a lock held by something
/// that never exits). Keepalive cannot see that: the connection keeps answering
/// pings, so it is never declared dead, and the stream is answered by pushes so
/// the request deadline is long since satisfied. Without this the calling
/// thread parks for the life of the process.
///
/// Set well past the slowest plausible gap between chunks — a cold-cache `git
/// diff` on a huge tree can take a while to print its first byte, and that gap
/// is the one being measured.
pub const GIT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// Ceiling on [`ControlRequest::GitStream`]s running at once on one connection.
///
/// Each is an OS thread of its own reading a `git` child, and unlike a request
/// it is not answered from the bounded worker pool — nothing else counts them.
/// One client asking honestly needs one or two (the diff overlay and the
/// Changes panel share a probe); this is set well above that and is a backstop
/// against a peer that asks faster than it drains, not a working limit.
///
/// A refused stream is answered with a failed [`ControlEvent::GitEnd`] rather
/// than dropped, because a stream that never speaks is the one shape a client
/// cannot recover from cheaply.
pub const MAX_CONCURRENT_GIT_STREAMS: usize = 8;

/// Capability strings advertised in [`crate::daemon::protocol::DaemonVersion::features`].
///
/// These exist so a capability added after protocol v3 doesn't need another
/// version bump: a peer answers "what can you do" with a list rather than a
/// number, and an unknown string is simply a capability this build won't use.
pub mod feature {
    /// Speaks the control dialect: kinds 60-63, this module's framing.
    pub const CONTROL: &str = "control";
    /// Serves [`super::ControlRequest`]'s filesystem and git methods — i.e. can
    /// back a remote `Host`. Distinct from [`CONTROL`] because a peer could
    /// speak the dialect while exposing only the workspace tree.
    pub const HOST_RPC: &str = "host-rpc";
    // `"workspace-store"` is a burned name: it advertised the retired
    // opaque-record scheme (verbs `workspace_list` / `workspace_get` /
    // `workspace_put` / `workspace_delete`, event `workspace_changed`), all of
    // which are burned with it. Never re-advertise or re-mint any of them with
    // a different meaning.
    /// Serves the machine-owned workspace tree: the `MachineGet` /
    /// `WorkspaceTree` pulls, the semantic tree operations, and the
    /// [`super::ControlEvent::Layout`] pushes. Advertised only when the server
    /// actually carries a [`crate::core::machine::MachineStore`], so a client
    /// learns from the handshake whether the tree verbs are worth a round
    /// trip.
    pub const MACHINE_TREE: &str = "machine-tree";
    /// Can be launched as `--stdio` and bridge its own stdin/stdout to the
    /// machine-local socket (the fallback when `AllowStreamLocalForwarding` is
    /// off, the only option under WSL, and how the CI end-to-end test runs).
    pub const STDIO_BRIDGE: &str = "stdio-bridge";
}

// ---------------------------------------------------------------------------
// Payload types
// ---------------------------------------------------------------------------

// The shapes that cross the wire in both directions — `Entry`, `Meta`,
// `MTime`, `Output`, `SearchHit` — are the `Host` trait's own types, re-exported
// here rather than mirrored. A parallel "wire" copy would be a standing invitation
// to let the two drift, and every drift between them is a silent
// mistranslation rather than a compile error.
pub use crate::host::{Entry, MTime, Meta, Output, SearchHit};

// Same rule for the machine's shell inventory: the dropdown's own type crosses
// the wire, not a wire-only copy of it.
pub use crate::core::shells::{DetectedShell, ShellInventory};

// And for the machine tree: the daemon's own tree types are the wire types, so
// a schema drift between the store and the dialect is a compile error rather
// than a silent mistranslation. `WorkspaceId` rides along because every tree
// verb addresses a workspace by it.
pub use crate::core::machine::{Axis, LayoutDelta, Machine, PaneSeed, Side, Tab, TabId};
pub use crate::core::session::WorkspaceId;

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// Everything a client can ask a control peer to do.
///
/// **Paths are `String`, never `PathBuf`.** `PathBuf`'s serde representation of
/// a non-UTF-8 path is platform-dependent, and the two ends of this connection
/// are routinely different operating systems. Remote paths are UTF-8 POSIX; a
/// non-UTF-8 name on the server is returned lossily by `ReadDir`, matching what
/// the file tree already does locally with `to_string_lossy`.
// Not `Eq`: the machine-tree verbs carry split ratios, and `f32` has no `Eq`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlRequest {
    // ----- liveness ---------------------------------------------------------
    Ping,

    // ----- filesystem reads -------------------------------------------------
    /// `root` bounds the gitignore chain: rules are evaluated from `root` down
    /// to `dir`, deeper wins, `!` re-includes. `None` means nothing is ignored
    /// except `.git`.
    ReadDir {
        dir: String,
        root: Option<String>,
    },
    Stat {
        path: String,
    },
    Exists {
        path: String,
    },
    Canonicalize {
        path: String,
    },
    /// `max_bytes` is enforced **on the server**: over the limit it answers
    /// [`WireErrorKind::FileTooLarge`] rather than shipping the file and having
    /// the client throw it away.
    ReadFile {
        path: String,
        max_bytes: u64,
    },
    /// Breadth-first substring match over names, run entirely on the server.
    /// The local implementation walks up to 2000 directories; doing that a
    /// directory at a time over a transcontinental link would be 2000 round
    /// trips.
    ///
    /// `show_hidden` is here rather than left to the client because it also
    /// governs *descent*: with it false the walk never enters an ignored or
    /// dot-prefixed directory, which is what stops `node_modules` from
    /// consuming the whole `max_dirs` budget. Filtering the hits afterwards on
    /// the client would give a different — and much slower — answer.
    Search {
        roots: Vec<String>,
        query: String,
        limit: u64,
        max_dirs: u64,
        show_hidden: bool,
    },

    // ----- filesystem writes ------------------------------------------------
    /// Content rides in the blob of a [`kind::REQUEST_BLOB`] frame.
    WriteFile {
        path: String,
    },
    /// Exclusive create; `AlreadyExists` if the path is taken (`File::create_new`).
    CreateFileNew {
        path: String,
    },
    /// `recursive` picks `create_dir_all` over `create_dir`.
    CreateDir {
        path: String,
        recursive: bool,
    },
    /// The server guarantees `AlreadyExists` when `to` exists — the client must
    /// not probe first, which would be both an extra round trip and a TOCTOU.
    Rename {
        from: String,
        to: String,
    },
    /// `recursive` only means anything for directories.
    Remove {
        path: String,
        recursive: bool,
    },

    // ----- git --------------------------------------------------------------
    /// The work-tree root containing `path`: nearest ancestor with a `.git`
    /// (directory or linked-worktree file). One server-side walk, not a ladder
    /// of round trips.
    RepoRoot {
        path: String,
    },
    /// `git -C <cwd> <args>`. A non-zero exit is a successful reply carrying
    /// that status, **not** an error — see [`WireError`].
    Git {
        cwd: String,
        args: Vec<String>,
    },
    /// `git -C <cwd> <args>`, delivered incrementally: the server pushes
    /// [`ControlEvent::GitChunk`]s under `id` until a [`ControlEvent::GitEnd`]
    /// carrying the exit status.
    ///
    /// Same invariants as [`Git`](Self::Git) — it is the same invocation — but
    /// neither side has to hold the whole output: `git diff HEAD` on a large
    /// work tree is tens of megabytes, and the caller throws most of it away.
    /// **The client picks `id`**, which is why there is no id in the reply.
    /// Ids only have to be unique within a connection, and one connection has
    /// one client — so choosing it here lets the client register its receiver
    /// *before* the request goes out. A server-assigned id would arrive in the
    /// reply, leaving a window in which a chunk that overtook it would reach a
    /// client with no entry for that id and be dropped under the unknown-id
    /// rule, silently losing the front of the diff.
    GitStream {
        id: u64,
        cwd: String,
        args: Vec<String>,
    },

    // ----- machine inventory -------------------------------------------------
    /// The shells installed on the server, for the new-tab dropdown of a window
    /// bound to it. Answered by every server speaking [`CONTROL_VERSION`] ≥ 2;
    /// an older one is replaced rather than asked (see there).
    Shells,

    // ----- watch ------------------------------------------------------------
    /// Open a subscription; the server answers with a [`ReplyOk::WatchId`].
    WatchOpen {
        dirs: Vec<String>,
    },
    /// Replace a subscription's directory set wholesale; the server diffs.
    WatchSet {
        id: u64,
        dirs: Vec<String>,
    },
    WatchClose {
        id: u64,
    },

    // The opaque record store's verbs — `workspace_list` / `workspace_get` /
    // `workspace_put` / `workspace_delete` — lived here until the machine tree
    // below replaced them. Their serde names are burned (see `feature`); do
    // not re-mint them with a different meaning.

    // ----- attachment (M6's takeover) ---------------------------------------
    /// Claim a workspace for this connection's session, taking it over from
    /// whoever held it. The server answers
    /// [`ReplyOk::Attached`] and pushes [`ControlEvent::Preempted`] to the
    /// displaced session.
    ///
    /// A *request* rather than only the [`ControlHello::workspace`] field
    /// because a client holds **one connection per machine** and may have
    /// several of that machine's workspaces open at once — a hello can name one
    /// workspace, and the second window would otherwise need a second link to
    /// the same box. Both paths run the identical server-side claim; the hello
    /// field remains the shorthand for a connection dedicated to one workspace,
    /// which is what the end-to-end tests use.
    WorkspaceAttach {
        id: String,
    },
    /// Release a workspace this connection holds. Token-checked on the server,
    /// so a session that has *already* been preempted cannot evict the client
    /// that took over from it by tidying up afterwards.
    WorkspaceDetach {
        id: String,
    },

    // ----- machine tree (the daemon-owned structure) ------------------------
    // The semantic replacement for the retired opaque record verbs: instead of
    // a whole-record `Put` (last-writer-wins the moment two clients write),
    // each operation names its edit, the server validates it against the tree
    // it owns, and everyone else hears an incremental
    // [`ControlEvent::Layout`]. Positions cross as `u64` for the same reason
    // `Search`'s limits do: a 32-bit server clamps rather than wraps.
    /// The whole tree — every workspace, tab and pane record on the machine.
    /// The full pull a client starts from before applying deltas.
    MachineGet,
    /// One workspace of the tree, whole. `NotFound` when the machine has no
    /// such workspace; also the re-pull a client falls back to when it cannot
    /// apply a delta.
    WorkspaceTree {
        workspace: WorkspaceId,
    },
    /// Create an empty workspace; its first tab arrives as its own operation.
    /// Answers the newborn [`ReplyOk::WorkspaceTree`]. `workspace` lets the
    /// client mint the id — a window names its workspace before any round trip
    /// completes — and `None` has the daemon mint one, as before. A taken id
    /// is refused, never adopted.
    WorkspaceCreate {
        name: Option<String>,
        #[serde(default)]
        workspace: Option<WorkspaceId>,
    },
    WorkspaceRename {
        workspace: WorkspaceId,
        name: Option<String>,
    },
    /// Forget a tree workspace and everything under it. Named `Remove` because
    /// `WorkspaceDelete` was the retired record store's verb, and its serde
    /// name stays burned.
    WorkspaceRemove {
        workspace: WorkspaceId,
    },
    /// Stamp a workspace as just-focused, for pickers ordered by recency.
    WorkspaceTouch {
        workspace: WorkspaceId,
    },
    WorkspaceSetActiveTab {
        workspace: WorkspaceId,
        tab: TabId,
    },
    /// Create a tab holding `pane` at position `at` (clamped; `None` appends).
    /// `pane` is the seed for a pane the client already spawned over the pane
    /// protocol — PTYs come from there, the tree only adopts them. `tab` is
    /// the client-minted identity (see `WorkspaceCreate::workspace`); `None`
    /// has the daemon mint one.
    TabCreate {
        workspace: WorkspaceId,
        at: Option<u64>,
        pane: PaneSeed,
        #[serde(default)]
        tab: Option<TabId>,
    },
    /// Close a tab. Answers [`ReplyOk::Panes`]: the pane ids that left the
    /// tree, for the caller to kill — the tree does bookkeeping, not process
    /// teardown.
    TabClose {
        workspace: WorkspaceId,
        tab: TabId,
    },
    TabRename {
        workspace: WorkspaceId,
        tab: TabId,
        name: Option<String>,
    },
    TabMove {
        workspace: WorkspaceId,
        tab: TabId,
        to: u64,
    },
    /// Record the tab's sidebar repo group, as resolved by the client.
    TabSetGroup {
        workspace: WorkspaceId,
        tab: TabId,
        group: Option<String>,
    },
    /// Split the leaf holding `pane`; `new` seeds the freshly-spawned second
    /// pane, `first` puts it on the upper/left side.
    PaneSplit {
        workspace: WorkspaceId,
        pane: u64,
        axis: Axis,
        ratio: f32,
        new: PaneSeed,
        first: bool,
    },
    /// Close one pane, collapsing its split (or the whole tab when it was the
    /// last pane). Answers [`ReplyOk::Panes`] like `TabClose`.
    PaneClose {
        workspace: WorkspaceId,
        pane: u64,
    },
    /// Move a split's divider. `path` addresses the split from the tab root,
    /// and a path the tree no longer has refuses rather than guessing.
    PaneSetRatio {
        workspace: WorkspaceId,
        tab: TabId,
        path: Vec<Side>,
        ratio: f32,
    },
    /// tmux's `move-pane`: take `pane` out of where it is and re-split it next
    /// to `to`, dissolving the source tab if that emptied it.
    PaneMove {
        workspace: WorkspaceId,
        pane: u64,
        to: u64,
        axis: Axis,
        first: bool,
    },
    /// The revival: rebind the leaf holding dead pane `old` to freshly-spawned
    /// successor `new`, spending the old registry record.
    PaneReplace {
        workspace: WorkspaceId,
        old: u64,
        new: PaneSeed,
    },
}

impl ControlRequest {
    /// How long the client waits before giving up on this request.
    ///
    /// Per-method rather than one global figure because the spread is real: a
    /// `stat` that hasn't answered in five seconds is not going to, while a
    /// `git status` on a cold large repo legitimately takes ten. A single
    /// conservative timeout would make the fast paths feel broken; a single
    /// aggressive one would break the slow paths.
    ///
    /// A timeout **never drops the connection**: the request fails with
    /// `TimedOut`, a [`kind::CANCEL`] goes out, and every other in-flight
    /// request is untouched.
    pub fn deadline(&self) -> Duration {
        use ControlRequest::*;
        match self {
            Ping => Duration::from_secs(5),
            ReadDir { .. }
            | Stat { .. }
            | Exists { .. }
            | Canonicalize { .. }
            | RepoRoot { .. }
            | WatchOpen { .. }
            | WatchSet { .. }
            | WatchClose { .. } => Duration::from_secs(5),
            ReadFile { .. } | WriteFile { .. } => Duration::from_secs(30),
            CreateFileNew { .. } | CreateDir { .. } | Rename { .. } | Remove { .. } => {
                Duration::from_secs(10)
            }
            // `GitStream`'s deadline covers only its *acceptance* — the reply is
            // immediate and the data arrives as pushes, which have their own
            // idle timeout (see `GIT_STREAM_IDLE_TIMEOUT`).
            Git { .. } | GitStream { .. } | Search { .. } => Duration::from_secs(20),
            // Filesystem probes on Unix, but on Windows the WSL enumeration
            // spawns `wsl.exe -l -q`, which is slow enough to deserve the same
            // budget as git.
            Shells => Duration::from_secs(20),
            // An attach is bookkeeping plus at most one push to a peer that may
            // be wedged — the push is `try`-shaped on the server, so this only
            // has to cover a slow link, not a slow client.
            WorkspaceAttach { .. } | WorkspaceDetach { .. } => Duration::from_secs(10),
            // Tree operations are a locked mutation plus one small file write,
            // so the budget covers a slow disk, not slow work.
            MachineGet
            | WorkspaceTree { .. }
            | WorkspaceCreate { .. }
            | WorkspaceRename { .. }
            | WorkspaceRemove { .. }
            | WorkspaceTouch { .. }
            | WorkspaceSetActiveTab { .. }
            | TabCreate { .. }
            | TabClose { .. }
            | TabRename { .. }
            | TabMove { .. }
            | TabSetGroup { .. }
            | PaneSplit { .. }
            | PaneClose { .. }
            | PaneSetRatio { .. }
            | PaneMove { .. }
            | PaneReplace { .. } => Duration::from_secs(10),
        }
    }

    /// Whether this request carries bulk bytes, i.e. rides
    /// [`kind::REQUEST_BLOB`] instead of [`kind::REQUEST`].
    pub fn takes_blob(&self) -> bool {
        matches!(self, ControlRequest::WriteFile { .. })
    }

    /// Whether this request's *reply* carries bulk bytes.
    pub fn returns_blob(&self) -> bool {
        matches!(self, ControlRequest::ReadFile { .. })
    }
}

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

/// A reply to one request: the operation's value, or why it couldn't run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ControlReply {
    #[serde(rename = "ok")]
    Ok(ReplyOk),
    #[serde(rename = "err")]
    Err(WireError),
}

impl ControlReply {
    /// Fold into the `io::Result` shape every `Host` method returns.
    pub fn into_result(self) -> io::Result<ReplyOk> {
        match self {
            ControlReply::Ok(v) => Ok(v),
            ControlReply::Err(e) => Err(e.into_io()),
        }
    }
}

/// The successful half of a reply. One variant per result *shape*, not per
/// request — several requests answer `Unit`, and `Stat` and `WriteFile` both
/// answer `Meta`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyOk {
    Unit,
    Pong,
    Entries(Vec<Entry>),
    Meta(Meta),
    Bool(bool),
    Path(String),
    OptPath(Option<String>),
    /// `ReadFile`: the content is the frame's blob, this is only its metadata.
    FileMeta {
        meta: Meta,
    },
    Hits(Vec<SearchHit>),
    Output(Output),
    WatchId(u64),
    /// [`ControlRequest::Shells`]: what that machine can launch.
    Shells(ShellInventory),
    /// [`ControlRequest::WorkspaceAttach`] succeeded. `took_over_from` names the
    /// machine whose session was displaced, so the client that *did* the taking
    /// can say so — only the notice going the other way is specified,
    /// but a takeover the new client cannot see is one the user cannot explain.
    Attached {
        took_over_from: Option<String>,
    },
    /// [`ControlRequest::MachineGet`]: the machine's whole tree. Boxed for the
    /// same reason the tree replies below are: `ReplyOk` values live on the
    /// dispatch stack, and the common replies must not pay for the big ones.
    MachineTree(Box<Machine>),
    /// One workspace of the tree ([`ControlRequest::WorkspaceTree`] /
    /// [`ControlRequest::WorkspaceCreate`]).
    WorkspaceTree(Box<crate::core::machine::Workspace>),
    /// The tab an operation created ([`ControlRequest::TabCreate`]).
    TabTree(Box<Tab>),
    /// Pane ids an operation removed from the tree, for the caller to kill.
    Panes(Vec<u64>),
}

/// An operation that could not be performed.
///
/// **A non-zero `git` exit is not this.** It is `Ok(Output { status: Some(1) })`.
/// That distinction is what lets `git_status`'s `Option<String>` semantics
/// survive the move behind the `Host` trait unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub kind: WireErrorKind,
    /// Human-readable, shown to the user verbatim. Carries the path when a path
    /// is the point, and nothing else about the server's internals.
    pub msg: String,
}

/// The closed set of error classes that cross the wire.
///
/// `io::ErrorKind` can't be used directly: it isn't `Serialize`, and it is
/// `#[non_exhaustive]`, so its variant set is not a stable wire contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireErrorKind {
    NotFound,
    PermissionDenied,
    AlreadyExists,
    InvalidInput,
    NotADirectory,
    IsADirectory,
    DirectoryNotEmpty,
    /// Past the caller's `max_bytes`.
    FileTooLarge,
    /// No git on the server, or it wouldn't start.
    GitUnavailable,
    TimedOut,
    /// The control connection died. Synthesized by the client; never sent.
    ConnectionReset,
    /// Anything else; `msg` carries the original description.
    Other,
}

impl WireError {
    pub fn new(kind: WireErrorKind, msg: impl Into<String>) -> Self {
        WireError {
            kind,
            msg: msg.into(),
        }
    }

    /// Classify an `io::Error` for the wire. The inverse of [`WireError::into_io`]
    /// on every kind this table names.
    pub fn from_io(e: &io::Error) -> Self {
        use io::ErrorKind as K;
        let kind = match e.kind() {
            K::NotFound => WireErrorKind::NotFound,
            K::PermissionDenied => WireErrorKind::PermissionDenied,
            K::AlreadyExists => WireErrorKind::AlreadyExists,
            K::InvalidInput | K::InvalidFilename => WireErrorKind::InvalidInput,
            K::NotADirectory => WireErrorKind::NotADirectory,
            K::IsADirectory => WireErrorKind::IsADirectory,
            K::DirectoryNotEmpty => WireErrorKind::DirectoryNotEmpty,
            K::FileTooLarge => WireErrorKind::FileTooLarge,
            K::TimedOut => WireErrorKind::TimedOut,
            K::ConnectionReset | K::BrokenPipe | K::UnexpectedEof => WireErrorKind::ConnectionReset,
            _ => WireErrorKind::Other,
        };
        WireError {
            kind,
            msg: e.to_string(),
        }
    }

    /// Rebuild a local `io::Error`. `msg` becomes the error's payload, so a
    /// notification shows what the server said rather than a generic class name.
    pub fn into_io(self) -> io::Error {
        io::Error::new(self.kind.to_io_kind(), self.msg)
    }
}

impl WireErrorKind {
    /// The `io::ErrorKind` this class maps back to.
    ///
    /// `GitUnavailable` lands on `NotFound` — it means the binary isn't there —
    /// and `Other` on `Other`, which is why the class is kept beside a `msg`.
    pub fn to_io_kind(self) -> io::ErrorKind {
        use io::ErrorKind as K;
        match self {
            WireErrorKind::NotFound | WireErrorKind::GitUnavailable => K::NotFound,
            WireErrorKind::PermissionDenied => K::PermissionDenied,
            WireErrorKind::AlreadyExists => K::AlreadyExists,
            WireErrorKind::InvalidInput => K::InvalidInput,
            WireErrorKind::NotADirectory => K::NotADirectory,
            WireErrorKind::IsADirectory => K::IsADirectory,
            WireErrorKind::DirectoryNotEmpty => K::DirectoryNotEmpty,
            WireErrorKind::FileTooLarge => K::FileTooLarge,
            WireErrorKind::TimedOut => K::TimedOut,
            WireErrorKind::ConnectionReset => K::ConnectionReset,
            WireErrorKind::Other => K::Other,
        }
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// An unsolicited server push, carried on [`kind::EVENT`] with `req_id == 0`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlEvent {
    /// Filesystem changes, coalesced and deduplicated by the server over a
    /// [`WATCH_COALESCE_WINDOW`].
    Watch {
        id: u64,
        paths: Vec<String>,
    },
    /// More than [`WATCH_BURST_CAP`] distinct paths changed in one window;
    /// the client invalidates the subtree instead of applying a list.
    WatchOverflow {
        id: u64,
    },

    /// A batch of a [`ControlRequest::GitStream`]'s stdout, in order.
    ///
    /// Line data, newline-terminated, batched into frames of roughly
    /// [`GIT_STREAM_CHUNK`] bytes — not a verbatim slice of git's stdout. The
    /// server reads through [`crate::host::Host::git_lines`], so what crosses
    /// the wire is what that yields: line *content* is preserved exactly (bar
    /// the lossy UTF-8 replacement it already applies), while `\r\n` endings
    /// and whether the last line carried a terminator are normalised away. The
    /// only consumer is line-oriented, so that is the contract rather than an
    /// accident — and framing per batch instead of per line is what keeps a
    /// 90 000-line diff from becoming 90 000 frames.
    ///
    /// Base64 for the same reason [`crate::host::Output::stdout`] is: events
    /// are encoded with `serde_json`, which has no byte type and renders one as
    /// an array of decimal numbers, so a plain `Vec<u8>` would put roughly four
    /// bytes on the wire per byte of diff — making the streaming path *more*
    /// expensive than the buffered read it exists to replace, on exactly the
    /// multi-megabyte diff it was added for.
    GitChunk {
        id: u64,
        #[serde(with = "crate::host::b64")]
        bytes: Vec<u8>,
    },
    /// The stream is over. `code` is git's exit status, `None` when it was
    /// killed by a signal; `failed` reports a read error on the server side,
    /// which is distinct from git exiting non-zero.
    GitEnd {
        id: u64,
        code: Option<i32>,
        failed: bool,
    },

    // ----- reserved for M5/M6; defined so the dialect doesn't need a bump ---
    PaneExited {
        pane_id: u64,
        code: Option<i32>,
    },
    AgentStatus {
        pane_id: u64,
        json: serde_json::Value,
    },
    /// The takeover: someone else attached to `workspace`, so this
    /// session no longer holds it.
    ///
    /// **`workspace` is not redundant.** One control connection carries a whole
    /// machine and may hold several of its workspaces, so a push that named only
    /// the new owner would leave the client unable to tell which of its windows
    /// just went read-only.
    Preempted {
        workspace: String,
        by: String,
    },
    // `workspace_changed` was the retired record store's change notice; its
    // serde name is burned along with the record verbs.
    /// One incremental change to one tree workspace on this machine — the
    /// push half of the machine-tree verbs. The writer never receives its own
    /// operation back (origin exclusion, so an optimistically-applied edit is
    /// not applied twice); every other client applies the delta to its live
    /// window or, when it cannot, re-pulls the workspace with
    /// [`ControlRequest::WorkspaceTree`].
    ///
    /// `workspace` is the [`WorkspaceId`] rendered as a string, matching how
    /// `Preempted` names its.
    Layout {
        workspace: String,
        delta: LayoutDelta,
    },
    /// The server dropped at least one [`Layout`](Self::Layout) push for this
    /// connection (its per-connection delta queue overflowed — see
    /// [`crate::host::server::LAYOUT_EVENT_QUEUE`]): the peer's mirrors are
    /// now wrong in a way no later delta repairs. So the client re-pulls the
    /// machine whole and resyncs its windows — the identical recovery a delta
    /// that will not apply already triggers, just server-announced instead of
    /// stumbled into. Connection-wide, because drops happen at the queue, not
    /// per workspace; the watch dialect's `WatchOverflow` is the precedent.
    ///
    /// It arrives *instead of* the deltas the queue was still holding, not
    /// ahead of them: those are older than the gap and already inside the tree
    /// the client is about to pull, so delivering them after the pull would
    /// walk the client backwards through history it has already left behind.
    LayoutResync,
}

/// Where control events that are nobody's *local* business end up.
///
/// [`RemoteHost`](crate::host::remote::RemoteHost) routes `Watch` and
/// `WatchOverflow` into the subscription that asked for them, because those
/// belong to a caller that is still holding a `WatchSub`. The rest —
/// `Preempted`, `PaneExited`, `AgentStatus`, `Layout` — are about a
/// *window*, and the host layer has no window.
///
/// A process-wide observer rather than a parameter on `connect_with` because
/// the interested party (the GUI's connection state machine) is one thing,
/// while connections are made in three places that would each have to be taught
/// to thread it through. Last registration wins; `None` drops events, which is
/// what a headless `tty7-server` wants.
pub type EventObserver = Arc<dyn Fn(crate::host::HostId, ControlEvent) + Send + Sync>;

static EVENT_OBSERVER: Mutex<Option<EventObserver>> = Mutex::new(None);

/// Install the observer. Idempotent, last-call-wins; the GUI calls it at
/// startup.
pub fn set_event_observer(f: EventObserver) {
    if let Ok(mut slot) = EVENT_OBSERVER.lock() {
        *slot = Some(f);
    }
}

/// Hand an unrouted event to the observer, if there is one.
///
/// **Runs on a reader thread**, so the observer must not block — the intended
/// shape is a mailbox push, exactly like the watch forwarders.
pub fn observe_event(host: crate::host::HostId, event: ControlEvent) {
    let observer = match EVENT_OBSERVER.lock() {
        Ok(slot) => slot.clone(),
        Err(_) => None,
    };
    match observer {
        Some(f) => f(host, event),
        None => log::trace!("control event with nobody to hear it: {event:?}"),
    }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// The client's opening frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlHello {
    /// The control dialect the client speaks. See [`CONTROL_VERSION`].
    pub control_version: u32,
    /// The workspace this connection is bound to. `None` uses the connection
    /// for host RPC only, which is how the stdio end-to-end test drives it.
    pub workspace: Option<String>,
    /// Session token and hostname, used later to decide takeover between two
    /// clients claiming the same workspace.
    pub client_token: String,
    pub client_hostname: String,
}

impl ControlHello {
    /// A hello for a connection that only does host RPC.
    pub fn host_rpc(client_token: impl Into<String>, client_hostname: impl Into<String>) -> Self {
        ControlHello {
            control_version: CONTROL_VERSION,
            workspace: None,
            client_token: client_token.into(),
            client_hostname: client_hostname.into(),
        }
    }
}

/// The server's answer. Sent **even when the versions don't match** — the
/// server then closes the connection, and this frame is the only way the client
/// learns which version it actually met (a `HELLO` has no `req_id`, so there is
/// no error reply to attach the mismatch to).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlHelloOk {
    pub control_version: u32,
    /// The server's [`crate::daemon::protocol::PROTOCOL_VERSION`]. Redundant
    /// with the control version; kept for diagnostics.
    pub protocol_version: u32,
    /// The server binary's version string. Display only.
    pub build: String,
    /// The server's path separator — this is what backs `Host::separator`, and
    /// why a Windows client can hold POSIX paths correctly.
    pub separator: char,
    /// The server's `$HOME`, so "new workspace" can default to `~` on the
    /// remote rather than on the client.
    pub home: String,
    /// Capability bits; see [`feature`].
    #[serde(default)]
    pub features: Vec<String>,
    /// Which *process* answered — see [`server_instance`].
    ///
    /// The one field here that changes without anything else changing. `build`
    /// is the same across a restart, and so are both version numbers, so before
    /// this a client that came back to a machine had no way to tell "the link
    /// blinked" from "the server is a different process now and every pane it
    /// held is gone". That question decides whether a reconnect re-attaches or
    /// rebuilds, and guessing it wrong either throws away live shells or leaves
    /// dead ones on screen.
    ///
    /// `#[serde(default)]` for the same reason every other added field has it,
    /// though nothing can currently send an empty one: control v2 is the floor
    /// and this landed with it. An empty value therefore means *unknown*, never
    /// "a server that restarted" — a client that cannot tell must not act as if
    /// it could.
    #[serde(default)]
    pub instance: String,
}

impl ControlHelloOk {
    /// Whether the server advertises `name`.
    pub fn has_feature(&self, name: &str) -> bool {
        self.features.iter().any(|f| f == name)
    }
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// `u64 req_id` + `u32 json_len`.
const CONTROL_HEADER: usize = 12;

fn to_json<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn from_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> io::Result<T> {
    serde_json::from_slice(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Build a `[req_id][json_len][JSON][blob]` payload.
fn encode_body(req_id: u64, json: &[u8], blob: &[u8]) -> io::Result<Vec<u8>> {
    let json_n = u32::try_from(json.len())
        .map_err(|_| invalid("control JSON exceeds the u32 length prefix"))?;
    let mut out = Vec::with_capacity(CONTROL_HEADER + json.len() + blob.len());
    out.extend_from_slice(&req_id.to_le_bytes());
    out.extend_from_slice(&json_n.to_le_bytes());
    out.extend_from_slice(json);
    out.extend_from_slice(blob);
    Ok(out)
}

/// Split a payload into `(req_id, json, blob)`.
///
/// Every bound is checked before a slice is taken: a hostile `json_len` must
/// produce an `InvalidData` error, never a panic and never a read past the
/// payload.
fn decode_body(payload: &[u8]) -> io::Result<(u64, &[u8], &[u8])> {
    if payload.len() < CONTROL_HEADER {
        return Err(invalid(format!(
            "control payload is {} bytes, shorter than the {CONTROL_HEADER}-byte header",
            payload.len()
        )));
    }
    let req_id = u64::from_le_bytes(payload[..8].try_into().unwrap());
    let json_n = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
    let json_end = CONTROL_HEADER
        .checked_add(json_n)
        .ok_or_else(|| invalid("control json_len overflows the payload offset"))?;
    if json_end > payload.len() {
        return Err(invalid(format!(
            "control json_len {json_n} runs past the {}-byte payload",
            payload.len()
        )));
    }
    Ok((
        req_id,
        &payload[CONTROL_HEADER..json_end],
        &payload[json_end..],
    ))
}

/// Decode a payload that must not have a trailing blob (kinds 61 and 63).
fn decode_body_exact<'a>(payload: &'a [u8], what: &str) -> io::Result<(u64, &'a [u8])> {
    let (req_id, json, blob) = decode_body(payload)?;
    if !blob.is_empty() {
        return Err(invalid(format!(
            "{what} carries {} trailing bytes; only the *_BLOB kinds may",
            blob.len()
        )));
    }
    Ok((req_id, json))
}

/// A request id of 0 is reserved for server pushes and is never valid on a
/// request, cancel or reply.
fn require_nonzero(req_id: u64, what: &str) -> io::Result<()> {
    if req_id == 0 {
        return Err(invalid(format!(
            "{what} used req_id 0, which is reserved for server pushes"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// A control frame travelling client → server.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlClientMsg {
    Hello(ControlHello),
    Request {
        req_id: u64,
        req: ControlRequest,
    },
    /// A request with bulk bytes; the JSON head still carries the parameters.
    RequestBlob {
        req_id: u64,
        req: ControlRequest,
        blob: Vec<u8>,
    },
    Cancel {
        req_id: u64,
    },
}

impl ControlClientMsg {
    /// Encode and write this message as one frame.
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let (k, payload) = self.to_frame()?;
        write_frame(w, k, &payload)
    }

    /// The frame this message *would* write, without writing it.
    ///
    /// Split out so a caller can tell "this message cannot be encoded" from
    /// "the link failed". Everything fallible here happens with the wire
    /// untouched, and the difference matters: marking the connection dead over
    /// a local serialization failure puts the workspace into `Reconnecting`
    /// over a request the server never saw.
    pub fn to_frame(&self) -> io::Result<(u8, Vec<u8>)> {
        let (k, payload) = match self {
            ControlClientMsg::Hello(hello) => (kind::HELLO, to_json(hello)?),
            ControlClientMsg::Request { req_id, req } => {
                require_nonzero(*req_id, "CONTROL_REQUEST")?;
                (kind::REQUEST, encode_body(*req_id, &to_json(req)?, &[])?)
            }
            ControlClientMsg::RequestBlob { req_id, req, blob } => {
                require_nonzero(*req_id, "CONTROL_REQUEST_BLOB")?;
                (
                    kind::REQUEST_BLOB,
                    encode_body(*req_id, &to_json(req)?, blob)?,
                )
            }
            ControlClientMsg::Cancel { req_id } => {
                require_nonzero(*req_id, "CONTROL_CANCEL")?;
                (kind::CANCEL, encode_body(*req_id, &[], &[])?)
            }
        };
        // See `ControlServerMsg::to_frame`: reaching the verdict here rather
        // than inside `write_frame` is what keeps "too big" a failure with the
        // wire untouched.
        if payload.len() > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "control frame of {} bytes exceeds the {MAX_FRAME}-byte limit",
                    payload.len()
                ),
            ));
        }
        Ok((k, payload))
    }

    /// Decode one already-read frame.
    pub fn from_frame(k: u8, payload: Vec<u8>) -> io::Result<Self> {
        match k {
            kind::HELLO => Ok(ControlClientMsg::Hello(from_json(&payload)?)),
            kind::REQUEST => {
                let (req_id, json) = decode_body_exact(&payload, "CONTROL_REQUEST")?;
                require_nonzero(req_id, "CONTROL_REQUEST")?;
                Ok(ControlClientMsg::Request {
                    req_id,
                    req: from_json(json)?,
                })
            }
            kind::REQUEST_BLOB => {
                let (req_id, json, blob) = decode_body(&payload)?;
                require_nonzero(req_id, "CONTROL_REQUEST_BLOB")?;
                Ok(ControlClientMsg::RequestBlob {
                    req_id,
                    req: from_json(json)?,
                    blob: blob.to_vec(),
                })
            }
            kind::CANCEL => {
                let (req_id, json) = decode_body_exact(&payload, "CONTROL_CANCEL")?;
                require_nonzero(req_id, "CONTROL_CANCEL")?;
                if !json.is_empty() {
                    return Err(invalid("CONTROL_CANCEL must carry an empty JSON head"));
                }
                Ok(ControlClientMsg::Cancel { req_id })
            }
            other => Err(invalid(format!("unknown ControlClientMsg kind {other}"))),
        }
    }

    /// Read and decode the next client message from `r`.
    pub fn read<R: Read>(r: &mut R) -> io::Result<Self> {
        let (k, payload) = read_frame(r)?;
        Self::from_frame(k, payload)
    }
}

/// A control frame travelling server → client.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlServerMsg {
    HelloOk(ControlHelloOk),
    Response {
        req_id: u64,
        reply: ControlReply,
    },
    /// A reply with bulk bytes; the JSON head carries the metadata.
    ResponseBlob {
        req_id: u64,
        reply: ControlReply,
        blob: Vec<u8>,
    },
    /// A push. Its `req_id` is always 0 on the wire.
    Event(ControlEvent),
}

impl ControlServerMsg {
    /// Encode and write this message as one frame.
    pub fn encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        let (k, payload) = self.to_frame()?;
        write_frame(w, k, &payload)
    }

    /// The frame this message *would* write, without writing it.
    ///
    /// The server's half of the same split as
    /// [`ControlClientMsg::to_frame`], and it earns its keep in the same way:
    /// a reply that cannot be encoded — a `SearchHit` path that is not UTF-8, a
    /// payload past [`MAX_FRAME`] — leaves the wire untouched, so the server can
    /// still answer the request with the error instead of dropping the reply and
    /// leaving the client to wait out its deadline.
    pub fn to_frame(&self) -> io::Result<(u8, Vec<u8>)> {
        let (k, payload) = match self {
            ControlServerMsg::HelloOk(ok) => (kind::HELLO_OK, to_json(ok)?),
            ControlServerMsg::Response { req_id, reply } => {
                require_nonzero(*req_id, "CONTROL_RESPONSE")?;
                (kind::RESPONSE, encode_body(*req_id, &to_json(reply)?, &[])?)
            }
            ControlServerMsg::ResponseBlob {
                req_id,
                reply,
                blob,
            } => {
                require_nonzero(*req_id, "CONTROL_RESPONSE_BLOB")?;
                (
                    kind::RESPONSE_BLOB,
                    encode_body(*req_id, &to_json(reply)?, blob)?,
                )
            }
            ControlServerMsg::Event(event) => (kind::EVENT, encode_body(0, &to_json(event)?, &[])?),
        };
        // Checked here rather than left to `write_frame`, so that "too big to
        // send" is a verdict reached with nothing written — which is what lets
        // the caller answer with an error instead of going silent.
        if payload.len() > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "control frame of {} bytes exceeds the {MAX_FRAME}-byte limit",
                    payload.len()
                ),
            ));
        }
        Ok((k, payload))
    }

    /// Decode one already-read frame.
    pub fn from_frame(k: u8, payload: Vec<u8>) -> io::Result<Self> {
        match k {
            kind::HELLO_OK => Ok(ControlServerMsg::HelloOk(from_json(&payload)?)),
            kind::RESPONSE => {
                let (req_id, json) = decode_body_exact(&payload, "CONTROL_RESPONSE")?;
                require_nonzero(req_id, "CONTROL_RESPONSE")?;
                Ok(ControlServerMsg::Response {
                    req_id,
                    reply: from_json(json)?,
                })
            }
            kind::RESPONSE_BLOB => {
                let (req_id, json, blob) = decode_body(&payload)?;
                require_nonzero(req_id, "CONTROL_RESPONSE_BLOB")?;
                Ok(ControlServerMsg::ResponseBlob {
                    req_id,
                    reply: from_json(json)?,
                    blob: blob.to_vec(),
                })
            }
            kind::EVENT => {
                let (req_id, json) = decode_body_exact(&payload, "CONTROL_EVENT")?;
                if req_id != 0 {
                    return Err(invalid(format!(
                        "CONTROL_EVENT carried req_id {req_id}; pushes must use 0"
                    )));
                }
                Ok(ControlServerMsg::Event(from_json(json)?))
            }
            other => Err(invalid(format!("unknown ControlServerMsg kind {other}"))),
        }
    }

    /// Read and decode the next server message from `r`.
    pub fn read<R: Read>(r: &mut R) -> io::Result<Self> {
        let (k, payload) = read_frame(r)?;
        Self::from_frame(k, payload)
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// How often the client pings, and only when the link has gone quiet — a busy
/// connection proves itself.
pub const KEEPALIVE_PING_INTERVAL: Duration = Duration::from_secs(15);
/// Silence that has to elapse before a ping is worth sending.
pub const KEEPALIVE_IDLE_BEFORE_PING: Duration = Duration::from_secs(30);
/// Silence after which the link counts as dead and the workspace goes to
/// `Reconnecting`. Three ping intervals: two may be lost without a false
/// positive.
pub const KEEPALIVE_DEAD_AFTER: Duration = Duration::from_secs(45);

/// How long [`ControlClient::close`] waits for its reader thread before
/// detaching it.
///
/// Generous enough that a reader woken by a real shutdown is always reaped
/// (that takes microseconds), short enough that a client built without a
/// [`LinkShutdown`] still closes promptly instead of hanging its caller.
pub const CLOSE_GRACE: Duration = Duration::from_millis(500);

/// A reply as the caller receives it: the value, plus the blob if the frame
/// carried one.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlResponse {
    pub reply: ReplyOk,
    pub blob: Vec<u8>,
}

/// Where server pushes go. Called on the reader thread, so it must not block —
/// the intended shape is a channel send.
pub type EventSink = Box<dyn Fn(ControlEvent) + Send + Sync + 'static>;

/// Whatever can force a parked reader out of a blocking `read`.
///
/// **This is not optional politeness; without it a client cannot be closed.**
/// The reader thread spends its whole life inside `read_frame`, which blocks
/// until the peer sends something. Setting a "we're closed now" flag does not
/// wake it, because nothing is looking at the flag — the thread is inside a
/// syscall. And the peer has no reason to send anything: it is waiting for the
/// next request. Both ends wait for the other forever.
///
/// So closing has to act on the file descriptor itself. Every transport has
/// *some* way to do that, but they share no trait in std — a socket has
/// `shutdown`, a child process has `kill`, an SSH channel has `close` — hence
/// this one-method abstraction rather than a bound on the stream type.
///
/// **Note for the server side** (the `Duplex`): the same problem
/// exists there in mirror image, so `Duplex` will want the same capability.
/// Aligning is a matter of `Duplex` either requiring `LinkShutdown` as a
/// supertrait or exposing an equivalent method; this trait is deliberately
/// minimal so it can be adopted rather than duplicated.
pub trait LinkShutdown: Send + Sync + 'static {
    /// Force the read half to return. Called at most once, and may be called
    /// while the reader is blocked inside `read`.
    fn shutdown_link(&self) -> io::Result<()>;
}

impl LinkShutdown for std::net::TcpStream {
    fn shutdown_link(&self) -> io::Result<()> {
        self.shutdown(std::net::Shutdown::Both)
    }
}

#[cfg(unix)]
impl LinkShutdown for std::os::unix::net::UnixStream {
    fn shutdown_link(&self) -> io::Result<()> {
        self.shutdown(std::net::Shutdown::Both)
    }
}

/// The client half of a control connection: one writer, one reader thread, and
/// a table of outstanding requests keyed by id.
///
/// **Out-of-order matching is the whole point.** Every call blocks its own
/// caller and nobody else's: a twenty-second `git` and a five-millisecond
/// `read_dir` issued in that order return in the opposite order, because the
/// reader claims each reply from the table by id rather than assuming replies
/// arrive as requests were sent. Without that, expanding a directory in the
/// file tree would queue behind whatever slow thing the status bar last asked
/// for.
///
/// Cloneable and `Sync`: `Arc<ControlClient>` is the intended way to share it,
/// and concurrent `call`s from many threads are the normal case.
pub struct ControlClient {
    inner: Arc<ClientInner>,
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
}

struct ClientInner {
    writer: Mutex<Box<dyn Write + Send>>,
    next_req_id: AtomicU64,
    pending: Mutex<HashMap<u64, SyncSender<ControlReply>>>,
    /// Blobs arrive on the same frame as their reply, so they ride a side table
    /// keyed by the same id rather than widening the channel's item type.
    blobs: Mutex<HashMap<u64, Vec<u8>>>,
    connected: AtomicBool,
    last_inbound: Mutex<Instant>,
    hello: ControlHelloOk,
    /// How to force the reader out of its blocking read. `None` when the caller
    /// supplied raw halves with no way to close them — then [`ControlClient::close`]
    /// detaches the reader instead of waiting for it.
    shutdown: Option<Arc<dyn LinkShutdown>>,
    /// Set by the reader as it exits, so `close` can wait for it *bounded*
    /// rather than joining a thread that may never return.
    reader_done: Mutex<bool>,
    reader_exit: Condvar,
    /// Run when the link is declared dead — see [`ControlClient::on_link_down`].
    link_down: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
}

impl ControlClient {
    /// Perform the handshake over an already-connected stream, then start the
    /// reader thread.
    ///
    /// `r` and `w` are the two halves of one duplex link — a `try_clone`d
    /// socket, or a child process's stdout and stdin. Taking them separately
    /// rather than behind a trait keeps this usable for both without this
    /// module having an opinion about how a stream splits.
    pub fn connect<R, W>(
        r: R,
        w: W,
        hello: &ControlHello,
        events: EventSink,
    ) -> io::Result<ControlClient>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        Self::connect_with(r, w, None, hello, events)
    }

    /// [`ControlClient::connect`] over a TCP socket, with shutdown wired up.
    ///
    /// Prefer this to `connect` whenever the transport *has* a shutdown: a
    /// client built without one cannot wake its own reader, so closing it costs
    /// a [`CLOSE_GRACE`] wait and leaks the reader thread until the peer
    /// happens to hang up.
    pub fn over_tcp(
        sock: std::net::TcpStream,
        hello: &ControlHello,
        events: EventSink,
    ) -> io::Result<ControlClient> {
        let r = sock.try_clone()?;
        let closer: Arc<dyn LinkShutdown> = Arc::new(sock.try_clone()?);
        Self::connect_with(r, sock, Some(closer), hello, events)
    }

    /// [`ControlClient::connect`] over a Unix-domain socket, with shutdown
    /// wired up.
    #[cfg(unix)]
    pub fn over_unix(
        sock: std::os::unix::net::UnixStream,
        hello: &ControlHello,
        events: EventSink,
    ) -> io::Result<ControlClient> {
        let r = sock.try_clone()?;
        let closer: Arc<dyn LinkShutdown> = Arc::new(sock.try_clone()?);
        Self::connect_with(r, sock, Some(closer), hello, events)
    }

    /// The full form: `shutdown` is what [`ControlClient::close`] uses to force
    /// the reader out of its blocking read. See [`LinkShutdown`] for why a flag
    /// alone cannot do it.
    pub fn connect_with<R, W>(
        mut r: R,
        mut w: W,
        shutdown: Option<Arc<dyn LinkShutdown>>,
        hello: &ControlHello,
        events: EventSink,
    ) -> io::Result<ControlClient>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        ControlClientMsg::Hello(hello.clone()).encode(&mut w)?;
        w.flush()?;

        let ok = match ControlServerMsg::read(&mut r)? {
            ControlServerMsg::HelloOk(ok) => ok,
            other => {
                return Err(invalid(format!(
                    "control peer answered the handshake with {other:?} instead of HELLO_OK"
                )));
            }
        };
        if ok.control_version != hello.control_version {
            // The peer has already closed by now; the frame existed purely so
            // this message can name both versions instead of saying "the
            // connection dropped".
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                dialect_refusal(&ok.build, ok.control_version, hello.control_version),
            ));
        }

        let inner = Arc::new(ClientInner {
            writer: Mutex::new(Box::new(w)),
            next_req_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            blobs: Mutex::new(HashMap::new()),
            connected: AtomicBool::new(true),
            last_inbound: Mutex::new(Instant::now()),
            hello: ok,
            shutdown,
            reader_done: Mutex::new(false),
            reader_exit: Condvar::new(),
            link_down: Mutex::new(Vec::new()),
        });

        let reader_inner = Arc::clone(&inner);
        let reader = std::thread::Builder::new()
            .name("tty7-control-reader".into())
            .spawn(move || reader_loop(reader_inner, r, events))?;

        Ok(ControlClient {
            inner,
            reader: Mutex::new(Some(reader)),
        })
    }

    /// What the peer said about itself.
    pub fn hello(&self) -> &ControlHelloOk {
        &self.inner.hello
    }

    /// Whether the link is still up. Once false it never returns to true — a
    /// reconnect builds a new `ControlClient`.
    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::Acquire)
    }

    /// Register work to run when the link is declared dead.
    ///
    /// `call` needs nothing of the sort — it waits on a slot in `pending`, and
    /// `fail_all` empties that. But a caller parked on a *push* is invisible
    /// there: [`ControlRequest::GitStream`]'s reply arrives long before its
    /// data does, so the thread draining the chunks is waiting on a channel
    /// this module has never heard of. Without a hook that channel is never
    /// closed and the thread parks until its idle timeout expires, holding
    /// whatever the caller believed was merely in flight.
    ///
    /// [`GIT_STREAM_IDLE_TIMEOUT`] does not make this redundant, and neither
    /// makes the other so: this fires the instant the link is known to be gone,
    /// where the timeout would sit out its full wait for an answer that is
    /// already impossible. The timeout covers the case a hook cannot see at all
    /// — a link that stays up while the far side stops producing.
    ///
    /// A hook may run more than once (`close` and the reader's own teardown
    /// both declare the link dead) and must therefore be idempotent, and it
    /// must not call back into this client — it runs while the hook table is
    /// held, on whichever thread noticed the link go.
    pub fn on_link_down<F: Fn() + Send + Sync + 'static>(&self, hook: F) {
        if let Ok(mut hooks) = self.inner.link_down.lock() {
            hooks.push(Box::new(hook));
        }
        // The link can die between the handshake and this call — a connection
        // refused mid-registration would otherwise leave the hook armed for an
        // event that has already passed.
        if !self.is_connected() {
            self.inner.run_link_down();
        }
    }

    /// How long the link has been silent. The caller's keepalive policy reads
    /// this against [`KEEPALIVE_IDLE_BEFORE_PING`] and
    /// [`KEEPALIVE_DEAD_AFTER`].
    pub fn idle_for(&self) -> Duration {
        self.inner
            .last_inbound
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }

    /// Issue a request and block until its reply, `deadline` elapses, or the
    /// link drops.
    ///
    /// Blocking is deliberate: `Host` is a blocking, object-safe
    /// trait, and every caller is already on a background thread. Blocking here
    /// blocks exactly one of them.
    pub fn call(&self, req: ControlRequest) -> io::Result<ReplyOk> {
        self.call_full(req, &[]).map(|r| r.reply)
    }

    /// [`ControlClient::call`] with bulk bytes attached to the request.
    pub fn call_with_blob(&self, req: ControlRequest, blob: &[u8]) -> io::Result<ReplyOk> {
        self.call_full(req, blob).map(|r| r.reply)
    }

    /// [`ControlClient::call`], keeping the reply's blob.
    pub fn call_full(&self, req: ControlRequest, blob: &[u8]) -> io::Result<ControlResponse> {
        let deadline = req.deadline();
        self.call_with_deadline(req, blob, deadline)
    }

    /// The full form, for callers that want a deadline other than the method's
    /// default (the conformance suite's tighter bounds, mainly).
    pub fn call_with_deadline(
        &self,
        req: ControlRequest,
        blob: &[u8],
        deadline: Duration,
    ) -> io::Result<ControlResponse> {
        if !self.is_connected() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "control connection is down",
            ));
        }

        let req_id = self.inner.next_req_id.fetch_add(1, Ordering::Relaxed);
        // Every request the client makes, named. A remote workspace's link is
        // invisible from the outside — the traffic is inside an SSH channel —
        // so without this the only evidence of *who* is talking is packet
        // lengths in russh's own trace, which is not evidence. Cheap: `Off` by
        // default, and the format cost only runs when it isn't.
        log::debug!(target: "tty7::control", "#{req_id} {req:?}");
        // Capacity 1: the reader must never block handing off a reply, and
        // there is only ever one reply per id.
        let (tx, rx) = sync_channel(1);
        self.inner.pending()?.insert(req_id, tx);

        let msg = if blob.is_empty() && !req.takes_blob() {
            ControlClientMsg::Request { req_id, req }
        } else {
            ControlClientMsg::RequestBlob {
                req_id,
                req,
                blob: blob.to_vec(),
            }
        };

        if let Err(e) = self.inner.send(&msg) {
            self.inner.forget(req_id);
            return Err(e);
        }

        match rx.recv_timeout(deadline) {
            Ok(reply) => {
                let blob = self.inner.take_blob(req_id);
                reply
                    .into_result()
                    .map(|reply| ControlResponse { reply, blob })
            }
            Err(RecvTimeoutError::Timeout) => {
                // Drop the slot first: a late reply then finds no entry and is
                // discarded, per the unknown-id rule.
                self.inner.forget(req_id);
                let _ = self.inner.send(&ControlClientMsg::Cancel { req_id });
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("control request {req_id} timed out after {deadline:?}"),
                ))
            }
            // The sender is gone: the reader shut down and cleared the table.
            Err(RecvTimeoutError::Disconnected) => {
                self.inner.forget(req_id);
                Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "control connection closed while the request was in flight",
                ))
            }
        }
    }

    /// Send a keepalive `Ping`, ignoring the answer's content.
    pub fn ping(&self) -> io::Result<()> {
        self.call(ControlRequest::Ping).map(|_| ())
    }

    /// Tear the link down: fail every waiter, force the reader out of its
    /// blocking read, and reap it.
    ///
    /// The order matters and so does the bound. Failing the waiters first means
    /// nobody is left holding a deadline against a socket that is about to go
    /// away. Shutting the link down is what actually wakes the reader —
    /// `fail_all` only touches this side's bookkeeping, and a reader parked in
    /// `read_frame` is inside a syscall where no flag can reach it.
    ///
    /// The join is bounded by [`CLOSE_GRACE`] because it must be. When no
    /// [`LinkShutdown`] was supplied there is nothing that *can* wake the
    /// reader, and an unbounded join would hang whichever thread dropped the
    /// client — in practice the UI thread closing a remote workspace. A
    /// detached reader thread is a far smaller problem than a frozen window: it
    /// exits on its own as soon as the peer hangs up, and it holds nothing but
    /// an `Arc` whose waiters have all been failed already.
    pub fn close(&self) {
        self.inner
            .fail_all("control connection closed by this client");
        if let Some(closer) = &self.inner.shutdown
            && let Err(e) = closer.shutdown_link()
        {
            // Already closed, or never opened. Either way the reader is on its
            // way out and there is nothing better to do.
            log::trace!("control link shutdown reported {e}");
        }

        let Ok(mut slot) = self.reader.lock() else {
            return;
        };
        let Some(handle) = slot.take() else { return };

        let reaped = match self.inner.reader_done.lock() {
            Ok(done) => self
                .inner
                .reader_exit
                .wait_timeout_while(done, CLOSE_GRACE, |done| !*done)
                .map(|(done, _)| *done)
                .unwrap_or(false),
            Err(_) => false,
        };
        if reaped {
            // The reader has already returned, so this join is immediate.
            let _ = handle.join();
        } else {
            log::debug!(
                "control reader did not exit within {CLOSE_GRACE:?}; detaching it \
                 rather than blocking the caller"
            );
            drop(handle);
        }
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        self.close();
    }
}

/// Hand-written because the writer is a trait object and the pending table
/// holds channel senders — neither of which can derive it, and neither of which
/// is what a reader of a log line wants anyway. What matters is who the peer is
/// and whether the link is alive.
impl std::fmt::Debug for ControlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let in_flight = self.inner.pending.lock().map(|p| p.len()).unwrap_or(0);
        f.debug_struct("ControlClient")
            .field("build", &self.inner.hello.build)
            .field("control_version", &self.inner.hello.control_version)
            .field("connected", &self.is_connected())
            .field("in_flight", &in_flight)
            .finish()
    }
}

impl ClientInner {
    fn pending(
        &self,
    ) -> io::Result<std::sync::MutexGuard<'_, HashMap<u64, SyncSender<ControlReply>>>> {
        self.pending
            .lock()
            .map_err(|_| io::Error::other("control request table was poisoned"))
    }

    fn send(&self, msg: &ControlClientMsg) -> io::Result<()> {
        // Encoded before the writer is even locked, and before anything is
        // concluded about the link. A message that cannot be serialized, or a
        // payload past `MAX_FRAME`, fails with the wire untouched — the server
        // never saw the request, so the connection is exactly as healthy as it
        // was and must not be marked dead. Only a failure *after* bytes could
        // have gone out leaves the stream in a state worth giving up on.
        let (k, payload) = msg.to_frame()?;

        let mut w = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("control writer was poisoned"))?;
        let r = write_frame(&mut *w, k, &payload).and_then(|()| w.flush());
        if r.is_err() {
            self.connected.store(false, Ordering::Release);
        }
        r
    }

    fn forget(&self, req_id: u64) {
        if let Ok(mut p) = self.pending.lock() {
            p.remove(&req_id);
        }
        if let Ok(mut b) = self.blobs.lock() {
            b.remove(&req_id);
        }
    }

    fn take_blob(&self, req_id: u64) -> Vec<u8> {
        self.blobs
            .lock()
            .ok()
            .and_then(|mut b| b.remove(&req_id))
            .unwrap_or_default()
    }

    /// Deliver a reply to whoever is waiting on `req_id`. A reply for an id
    /// nobody is waiting on is dropped without complaint: it is the expected
    /// tail of a request that already timed out.
    fn deliver(&self, req_id: u64, reply: ControlReply, blob: Vec<u8>) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let Some(tx) = pending.remove(&req_id) else {
            log::trace!("control reply for unknown req_id {req_id}; dropping");
            return;
        };
        // Under the `pending` lock, which is the one `forget` takes first.
        // Inserting after releasing it loses the race with a caller that times
        // out in the gap: its `forget` clears a `blobs` entry that does not
        // exist yet, this insert lands with nobody left to take it, and a whole
        // file's contents stay in the map for the life of the connection.
        if !blob.is_empty()
            && let Ok(mut blobs) = self.blobs.lock()
        {
            blobs.insert(req_id, blob);
        }
        drop(pending);
        // Capacity 1 and one reply per id, so this cannot block; it can only
        // fail if the caller already gave up between the table lookup and here.
        let _ = tx.try_send(reply);
    }

    /// Fail every outstanding request. Called when the link dies, so callers
    /// get `ConnectionReset` immediately instead of each waiting out its own
    /// deadline.
    ///
    /// Waiters on a *push* rather than a reply are not in `pending` and cannot
    /// be woken from here, so the hooks run too — see
    /// [`ControlClient::on_link_down`].
    fn fail_all(&self, why: &str) {
        self.connected.store(false, Ordering::Release);
        let drained: Vec<_> = match self.pending.lock() {
            Ok(mut p) => p.drain().collect(),
            Err(_) => Vec::new(),
        };
        for (req_id, tx) in drained {
            let _ = tx.try_send(ControlReply::Err(WireError::new(
                WireErrorKind::ConnectionReset,
                format!("{why} (request {req_id})"),
            )));
        }
        self.run_link_down();
    }

    /// Run every registered link-down hook. Idempotent by contract, since both
    /// `close` and the reader's own teardown reach `fail_all`.
    fn run_link_down(&self) {
        if let Ok(hooks) = self.link_down.lock() {
            for hook in hooks.iter() {
                hook();
            }
        }
    }
}

fn reader_loop<R: Read>(inner: Arc<ClientInner>, r: R, events: EventSink) {
    read_until_closed(&inner, r, events);
    // Whatever ended the loop, `close` is entitled to stop waiting.
    if let Ok(mut done) = inner.reader_done.lock() {
        *done = true;
    }
    inner.reader_exit.notify_all();
}

fn read_until_closed<R: Read>(inner: &Arc<ClientInner>, mut r: R, events: EventSink) {
    loop {
        let frame = match read_frame(&mut r) {
            Ok(f) => f,
            Err(e) => {
                log::debug!("control reader stopping: {e}");
                inner.fail_all("control connection lost");
                return;
            }
        };
        if let Ok(mut t) = inner.last_inbound.lock() {
            *t = Instant::now();
        }
        match ControlServerMsg::from_frame(frame.0, frame.1) {
            Ok(ControlServerMsg::Response { req_id, reply }) => {
                inner.deliver(req_id, reply, Vec::new());
            }
            Ok(ControlServerMsg::ResponseBlob {
                req_id,
                reply,
                blob,
            }) => {
                inner.deliver(req_id, reply, blob);
            }
            Ok(ControlServerMsg::Event(event)) => events(event),
            Ok(ControlServerMsg::HelloOk(_)) => {
                // A second handshake mid-stream is a desync, not a greeting.
                log::warn!("control peer sent a second HELLO_OK; dropping the connection");
                inner.fail_all("control peer re-sent its handshake");
                return;
            }
            Err(e) => {
                // A frame we cannot parse means the stream position is no
                // longer trustworthy — same verdict the pane protocol reaches.
                log::warn!("control decode error: {e}");
                inner.fail_all("control stream desynchronized");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::MAX_FRAME;
    use std::io::Cursor;
    use std::path::PathBuf;

    fn meta() -> Meta {
        Meta {
            is_dir: false,
            is_symlink: false,
            len: 4096,
            mtime: Some(MTime {
                secs: 1_769_000_000,
                nanos: 123_456_789,
            }),
            readonly: false,
        }
    }

    /// A bare `ClientInner` with no link behind it — enough for the side-table
    /// bookkeeping, which is all `deliver`/`forget` touch.
    fn detached_inner() -> Arc<ClientInner> {
        Arc::new(ClientInner {
            writer: Mutex::new(Box::new(Cursor::new(Vec::new()))),
            next_req_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            blobs: Mutex::new(HashMap::new()),
            connected: AtomicBool::new(true),
            last_inbound: Mutex::new(Instant::now()),
            hello: ControlHelloOk {
                control_version: CONTROL_VERSION,
                protocol_version: 0,
                build: "test".into(),
                separator: '/',
                home: "/home/me".into(),
                features: Vec::new(),
                instance: "test-instance".into(),
            },
            shutdown: None,
            reader_done: Mutex::new(false),
            reader_exit: Condvar::new(),
            link_down: Mutex::new(Vec::new()),
        })
    }

    /// A path its own platform accepts but `str` cannot hold — Latin-1 bytes on
    /// Unix, an unpaired surrogate on Windows. Both are real filenames, and
    /// neither survives a JSON wire.
    fn unrepresentable_path() -> PathBuf {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/caf\xe9.rs"))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt as _;
            // `C:\` then a lone high surrogate: legal in an NTFS name, not
            // convertible to a `str`.
            PathBuf::from(std::ffi::OsString::from_wide(&[
                0x0043, 0x003a, 0x005c, 0xd800, 0x002e, 0x0072, 0x0073,
            ]))
        }
    }

    /// `to_frame` reaches every "cannot be sent" verdict with the wire
    /// untouched. That is what lets the server answer such a reply with an
    /// error instead of dropping it, and the client keep its link on a local
    /// encode failure — both of which are silent hangs otherwise.
    #[test]
    fn to_frame_refuses_what_cannot_be_sent_without_writing_it() {
        // A path the platform accepts and `str` cannot hold. `serde` errors on
        // such a `Path` rather than converting it lossily, which is the shape a
        // `SearchHit` takes when the server has one under the search roots.
        let unrepresentable = ControlServerMsg::Response {
            req_id: 1,
            reply: ControlReply::Ok(ReplyOk::Hits(vec![SearchHit {
                name: "caf\u{fffd}.rs".into(),
                path: unrepresentable_path(),
                is_dir: false,
                ignored: false,
            }])),
        };
        assert!(
            unrepresentable.to_frame().is_err(),
            "a path that is not valid Unicode must not encode"
        );

        // And the size verdict, which `write_frame` would otherwise reach only
        // after the writer was locked.
        let huge = ControlServerMsg::ResponseBlob {
            req_id: 1,
            reply: ControlReply::Ok(ReplyOk::Meta(meta())),
            blob: vec![0u8; MAX_FRAME + 1],
        };
        let err = huge
            .to_frame()
            .expect_err("an oversize reply must not encode");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// A message that cannot be put on the wire is not a dead link. `send`
    /// fails before the writer is even locked, so the server never saw the
    /// request and the connection is exactly as healthy as it was — marking it
    /// dead would drop the workspace into `Reconnecting` and, since
    /// `is_connected` never returns to true, keep it there.
    #[test]
    fn a_frame_too_large_to_send_does_not_condemn_the_link() {
        let inner = detached_inner();
        let oversize = ControlClientMsg::RequestBlob {
            req_id: 1,
            req: ControlRequest::WriteFile {
                path: "/home/me/huge.bin".into(),
            },
            blob: vec![0u8; MAX_FRAME + 1],
        };

        let err = inner
            .send(&oversize)
            .expect_err("an oversize frame must fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            inner.connected.load(Ordering::Acquire),
            "a local encode failure marked the connection dead"
        );

        // And the link still works for a message that does fit.
        inner.send(&ControlClientMsg::Cancel { req_id: 1 }).unwrap();
    }

    /// A caller giving up at the same instant its reply lands must not leave
    /// the blob behind. `deliver` and `forget` both touch two tables, and if
    /// the blob goes in after the `pending` lock is released, `forget` clears
    /// an entry that does not exist yet and the bytes — a whole file, up to the
    /// editor's limit — are retained for the life of the connection.
    ///
    /// Hammered rather than interleaved by hand: there is no seam to inject at,
    /// so this leans on repetition to hit the window. It fails intermittently
    /// against the insert-after-unlock ordering and never against the fix.
    #[test]
    fn a_blob_racing_its_callers_timeout_is_not_retained() {
        for req_id in 1..2_000u64 {
            let inner = detached_inner();
            let (tx, _rx) = std::sync::mpsc::sync_channel(1);
            inner.pending.lock().unwrap().insert(req_id, tx);

            let giving_up = Arc::clone(&inner);
            let t = std::thread::spawn(move || giving_up.forget(req_id));
            inner.deliver(req_id, ControlReply::Ok(ReplyOk::Unit), vec![7u8; 64]);
            t.join().unwrap();

            assert!(
                inner.blobs.lock().unwrap().is_empty(),
                "a blob outlived the request it belonged to (req_id {req_id})"
            );
        }
    }

    fn every_request() -> Vec<ControlRequest> {
        vec![
            ControlRequest::Ping,
            ControlRequest::ReadDir {
                dir: "/home/me/proj/src".into(),
                root: Some("/home/me/proj".into()),
            },
            ControlRequest::ReadDir {
                dir: "/tmp".into(),
                root: None,
            },
            ControlRequest::Stat {
                path: "/home/me/.zshrc".into(),
            },
            ControlRequest::Exists {
                path: "/nope".into(),
            },
            ControlRequest::Canonicalize {
                path: "/a/../b".into(),
            },
            ControlRequest::ReadFile {
                path: "/home/me/proj/Cargo.toml".into(),
                max_bytes: 10 * 1024 * 1024,
            },
            ControlRequest::Search {
                roots: vec!["/home/me/proj".into(), "/srv".into()],
                query: "widget".into(),
                limit: 200,
                max_dirs: 2000,
                show_hidden: false,
            },
            ControlRequest::WriteFile {
                path: "/home/me/proj/src/main.rs".into(),
            },
            ControlRequest::CreateFileNew {
                path: "/home/me/new.txt".into(),
            },
            ControlRequest::CreateDir {
                path: "/home/me/a/b/c".into(),
                recursive: true,
            },
            ControlRequest::Rename {
                from: "/a".into(),
                to: "/b".into(),
            },
            ControlRequest::Remove {
                path: "/home/me/tmp".into(),
                recursive: true,
            },
            ControlRequest::RepoRoot {
                path: "/home/me/proj/src/deep".into(),
            },
            ControlRequest::Git {
                cwd: "/home/me/proj".into(),
                args: vec!["status".into(), "--porcelain".into()],
            },
            ControlRequest::WatchOpen {
                dirs: vec!["/home/me/proj".into()],
            },
            ControlRequest::WatchSet {
                id: 7,
                dirs: vec!["/home/me/proj".into(), "/home/me/proj/src".into()],
            },
            ControlRequest::WatchClose { id: 7 },
        ]
    }

    fn every_reply() -> Vec<ControlReply> {
        vec![
            ControlReply::Ok(ReplyOk::Unit),
            ControlReply::Ok(ReplyOk::Pong),
            ControlReply::Ok(ReplyOk::Entries(vec![
                Entry {
                    name: "src".into(),
                    is_dir: true,
                    is_symlink: false,
                    ignored: false,
                },
                Entry {
                    name: "target".into(),
                    is_dir: true,
                    is_symlink: false,
                    ignored: true,
                },
                Entry {
                    name: "link".into(),
                    is_dir: false,
                    is_symlink: true,
                    ignored: false,
                },
            ])),
            ControlReply::Ok(ReplyOk::Meta(meta())),
            ControlReply::Ok(ReplyOk::Bool(true)),
            ControlReply::Ok(ReplyOk::Bool(false)),
            ControlReply::Ok(ReplyOk::Path("/home/me/proj".into())),
            ControlReply::Ok(ReplyOk::OptPath(Some("/home/me/proj".into()))),
            ControlReply::Ok(ReplyOk::OptPath(None)),
            ControlReply::Ok(ReplyOk::FileMeta { meta: meta() }),
            ControlReply::Ok(ReplyOk::Hits(vec![SearchHit {
                name: "widget.rs".into(),
                path: PathBuf::from("/home/me/proj/src/widget.rs"),
                is_dir: false,
                ignored: false,
            }])),
            ControlReply::Ok(ReplyOk::Output(Output {
                status: Some(1),
                stdout: b"?? src/new.rs\n".to_vec(),
                stderr: vec![0x00, 0xff, 0xfe, b'\n'],
            })),
            ControlReply::Ok(ReplyOk::WatchId(42)),
            ControlReply::Err(WireError::new(WireErrorKind::NotFound, "no such file")),
            ControlReply::Err(WireError::new(
                WireErrorKind::PermissionDenied,
                "permission denied",
            )),
            ControlReply::Err(WireError::new(
                WireErrorKind::AlreadyExists,
                "already exists",
            )),
            ControlReply::Err(WireError::new(WireErrorKind::InvalidInput, "bad path")),
            ControlReply::Err(WireError::new(WireErrorKind::NotADirectory, "not a dir")),
            ControlReply::Err(WireError::new(WireErrorKind::IsADirectory, "is a dir")),
            ControlReply::Err(WireError::new(
                WireErrorKind::DirectoryNotEmpty,
                "not empty",
            )),
            ControlReply::Err(WireError::new(WireErrorKind::FileTooLarge, "too large")),
            ControlReply::Err(WireError::new(WireErrorKind::GitUnavailable, "no git")),
            ControlReply::Err(WireError::new(WireErrorKind::TimedOut, "timed out")),
            ControlReply::Err(WireError::new(WireErrorKind::ConnectionReset, "reset")),
            ControlReply::Err(WireError::new(WireErrorKind::Other, "something else")),
        ]
    }

    fn every_event() -> Vec<ControlEvent> {
        vec![
            ControlEvent::Watch {
                id: 3,
                paths: vec!["/home/me/proj/src/main.rs".into()],
            },
            ControlEvent::WatchOverflow { id: 3 },
            ControlEvent::PaneExited {
                pane_id: 9,
                code: Some(0),
            },
            ControlEvent::PaneExited {
                pane_id: 9,
                code: None,
            },
            ControlEvent::AgentStatus {
                pane_id: 9,
                json: serde_json::json!({ "state": "thinking" }),
            },
            ControlEvent::Preempted {
                workspace: "w1".into(),
                by: "other-laptop".into(),
            },
        ]
    }

    fn hello() -> ControlHello {
        ControlHello {
            control_version: CONTROL_VERSION,
            workspace: Some("w1".into()),
            client_token: "tok".into(),
            client_hostname: "laptop".into(),
        }
    }

    /// One id for the whole process. A per-connection id would make every
    /// reconnect look like a restart, which is the failure this whole mechanism
    /// exists to avoid — in the *expensive* direction, since the client answers
    /// a restart by rebuilding the window.
    #[test]
    fn the_server_instance_is_one_value_per_process() {
        let first = server_instance();
        assert!(!first.is_empty(), "an empty instance means \"unknown\"");
        assert_eq!(first, server_instance());
    }

    /// A client on an older v2 build decodes a hello that has no `instance` as
    /// "unknown" rather than failing the whole handshake.
    #[test]
    fn a_hello_without_an_instance_still_decodes() {
        let json = r#"{"control_version":2,"protocol_version":3,"build":"26.7.6",
                       "separator":"/","home":"/home/me"}"#;
        let ok: ControlHelloOk = serde_json::from_str(json).expect("decodes without instance");
        assert_eq!(ok.instance, "");
        assert!(ok.features.is_empty());
    }

    fn hello_ok() -> ControlHelloOk {
        ControlHelloOk {
            control_version: CONTROL_VERSION,
            protocol_version: crate::daemon::protocol::PROTOCOL_VERSION,
            build: "26.7.5".into(),
            separator: '/',
            home: "/home/me".into(),
            features: vec![feature::CONTROL.into(), feature::HOST_RPC.into()],
            instance: "test-instance".into(),
        }
    }

    /// Every client-direction message survives encode → read, including one
    /// `Request` per `ControlRequest` variant.
    #[test]
    fn client_messages_round_trip() {
        let mut msgs = vec![
            ControlClientMsg::Hello(hello()),
            ControlClientMsg::Hello(ControlHello::host_rpc("tok", "laptop")),
            ControlClientMsg::Cancel { req_id: 1 },
            ControlClientMsg::Cancel { req_id: u64::MAX },
            ControlClientMsg::RequestBlob {
                req_id: 5,
                req: ControlRequest::WriteFile {
                    path: "/tmp/x".into(),
                },
                blob: vec![0x00, 0xff, b'h', b'i'],
            },
            ControlClientMsg::RequestBlob {
                req_id: 6,
                req: ControlRequest::WriteFile {
                    path: "/tmp/empty".into(),
                },
                blob: Vec::new(),
            },
        ];
        for (i, req) in every_request().into_iter().enumerate() {
            msgs.push(ControlClientMsg::Request {
                req_id: i as u64 + 1,
                req,
            });
        }

        for msg in &msgs {
            let mut buf = Vec::new();
            msg.encode(&mut buf).unwrap();
            let got = ControlClientMsg::read(&mut Cursor::new(&buf)).unwrap();
            assert_eq!(&got, msg, "client message did not survive the round trip");
        }
    }

    /// Every server-direction message survives encode → read, including one
    /// `Response` per `ControlReply` variant and one `Event` per
    /// `ControlEvent`.
    #[test]
    fn server_messages_round_trip() {
        let mut msgs = vec![
            ControlServerMsg::HelloOk(hello_ok()),
            ControlServerMsg::ResponseBlob {
                req_id: 9,
                reply: ControlReply::Ok(ReplyOk::FileMeta { meta: meta() }),
                blob: b"file contents\0\xff".to_vec(),
            },
            ControlServerMsg::ResponseBlob {
                req_id: 10,
                reply: ControlReply::Ok(ReplyOk::FileMeta { meta: meta() }),
                blob: Vec::new(),
            },
        ];
        for (i, reply) in every_reply().into_iter().enumerate() {
            msgs.push(ControlServerMsg::Response {
                req_id: i as u64 + 1,
                reply,
            });
        }
        for event in every_event() {
            msgs.push(ControlServerMsg::Event(event));
        }

        for msg in &msgs {
            let mut buf = Vec::new();
            msg.encode(&mut buf).unwrap();
            let got = ControlServerMsg::read(&mut Cursor::new(&buf)).unwrap();
            assert_eq!(&got, msg, "server message did not survive the round trip");
        }
    }

    /// The byte layout is the contract's, checked by hand rather than by
    /// round-tripping through our own decoder — a symmetric bug in both halves
    /// would pass every round-trip test and still be unreadable to the other
    /// implementation.
    #[test]
    fn frame_layout_matches_the_contract() {
        let mut buf = Vec::new();
        ControlClientMsg::RequestBlob {
            req_id: 0x0102_0304_0506_0708,
            req: ControlRequest::WriteFile { path: "/x".into() },
            blob: vec![0xaa, 0xbb],
        }
        .encode(&mut buf)
        .unwrap();

        // Outer frame: [u32 LE payload_len][u8 kind][payload]
        let payload_len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        assert_eq!(buf[4], 62, "REQUEST_BLOB is kind 62");
        assert_eq!(
            buf.len(),
            5 + payload_len,
            "frame length covers the payload"
        );

        let payload = &buf[5..];
        assert_eq!(
            &payload[..8],
            &0x0102_0304_0506_0708u64.to_le_bytes(),
            "req_id is 8 bytes, little-endian, first"
        );
        let json_n = u32::from_le_bytes(payload[8..12].try_into().unwrap()) as usize;
        let json = &payload[12..12 + json_n];
        assert_eq!(
            serde_json::from_slice::<ControlRequest>(json).unwrap(),
            ControlRequest::WriteFile { path: "/x".into() }
        );
        assert_eq!(&payload[12 + json_n..], &[0xaa, 0xbb], "blob is the tail");
        assert_eq!(
            payload_len,
            12 + json_n + 2,
            "payload_len == 12 + json_n + blob_len"
        );
    }

    /// A non-blob frame's `payload_len` is exactly `12 + json_n`, with nothing
    /// trailing.
    #[test]
    fn non_blob_frames_have_no_tail() {
        let mut buf = Vec::new();
        ControlClientMsg::Request {
            req_id: 1,
            req: ControlRequest::Ping,
        }
        .encode(&mut buf)
        .unwrap();
        let payload_len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        let json_n = u32::from_le_bytes(buf[13..17].try_into().unwrap()) as usize;
        assert_eq!(payload_len, 12 + json_n);
    }

    /// `CONTROL_CANCEL` is a bare `[req_id][json_len = 0]` — twelve bytes, no
    /// JSON at all.
    #[test]
    fn cancel_is_twelve_bytes_with_an_empty_json_head() {
        let mut buf = Vec::new();
        ControlClientMsg::Cancel { req_id: 77 }
            .encode(&mut buf)
            .unwrap();
        assert_eq!(u32::from_le_bytes(buf[..4].try_into().unwrap()), 12);
        assert_eq!(buf[4], 63);
        assert_eq!(&buf[5..13], &77u64.to_le_bytes());
        assert_eq!(&buf[13..17], &0u32.to_le_bytes());
        assert_eq!(buf.len(), 17);
    }

    /// A git chunk's bytes cross the wire as base64, not as a JSON array of
    /// decimal numbers.
    ///
    /// The difference is roughly four wire bytes per byte of diff, on the one
    /// path whose entire reason for existing is a multi-megabyte read — and it
    /// is invisible in a plain round-trip assertion, because the array form
    /// decodes back perfectly well. So the size is asserted too: 8 KiB of
    /// printable text cannot fit in under twice its length as decimals and
    /// commas, and does fit comfortably as base64.
    #[test]
    fn a_git_chunk_crosses_the_wire_as_base64() {
        let bytes: Vec<u8> = (0..8192u32).map(|i| b'a' + (i % 26) as u8).collect();
        let event = ControlEvent::GitChunk {
            id: 7,
            bytes: bytes.clone(),
        };
        let json = to_json(&event).unwrap();
        assert!(
            json.len() < bytes.len() * 2,
            "a {}-byte chunk encoded to {} bytes of JSON — the array-of-numbers \
             encoding is back",
            bytes.len(),
            json.len()
        );

        let (k, payload) = ControlServerMsg::Event(event).to_frame().unwrap();
        match ControlServerMsg::from_frame(k, payload).unwrap() {
            ControlServerMsg::Event(ControlEvent::GitChunk { id, bytes: got }) => {
                assert_eq!(id, 7);
                assert_eq!(got, bytes, "the bytes survive the encoding exactly");
            }
            other => panic!("expected a GitChunk, got {other:?}"),
        }
    }

    /// The chunk ceiling leaves room for what encoding costs. A chunk capped at
    /// [`GIT_STREAM_CHUNK_MAX`] grows by base64's four-thirds before the JSON
    /// envelope goes round it, so a ceiling set anywhere near [`MAX_FRAME`]
    /// would produce chunks that cannot be sent at all — the failure this cap
    /// exists to make impossible.
    #[test]
    fn the_git_chunk_ceiling_leaves_room_for_base64_and_the_envelope() {
        let encoded = GIT_STREAM_CHUNK_MAX / 3 * 4 + 4;
        assert!(
            encoded + 4096 < MAX_FRAME,
            "a full chunk encodes to {encoded} bytes against a {MAX_FRAME}-byte frame limit"
        );
        const {
            assert!(
                GIT_STREAM_CHUNK_MAX > GIT_STREAM_CHUNK,
                "the ceiling is a backstop for an oversized line, not the batch target"
            )
        };
    }

    /// An event's `req_id` is on the wire and is zero — the eight redundant
    /// bytes that let one header parser serve every non-`HELLO` kind.
    #[test]
    fn events_carry_a_zero_req_id() {
        let mut buf = Vec::new();
        ControlServerMsg::Event(ControlEvent::WatchOverflow { id: 1 })
            .encode(&mut buf)
            .unwrap();
        assert_eq!(buf[4], 63);
        assert_eq!(&buf[5..13], &0u64.to_le_bytes());
    }

    // ---- hostile / malformed input -----------------------------------------

    /// A frame shorter than the 12-byte control header errors instead of
    /// slicing out of bounds.
    #[test]
    fn short_payload_is_invalid_data() {
        for len in 0..CONTROL_HEADER {
            let payload = vec![0u8; len];
            for k in [kind::REQUEST, kind::REQUEST_BLOB, kind::CANCEL] {
                let e = ControlClientMsg::from_frame(k, payload.clone()).unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::InvalidData, "kind {k}, len {len}");
            }
            for k in [kind::RESPONSE, kind::RESPONSE_BLOB, kind::EVENT] {
                let e = ControlServerMsg::from_frame(k, payload.clone()).unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::InvalidData, "kind {k}, len {len}");
            }
        }
    }

    /// A `json_len` that runs past the payload — including `u32::MAX` — errors
    /// rather than panicking on the slice. This is the one field a hostile peer
    /// controls that indexes memory.
    #[test]
    fn oversized_json_len_is_invalid_data_not_a_panic() {
        for json_n in [13u32, 1_000_000, u32::MAX] {
            let mut payload = Vec::new();
            payload.extend_from_slice(&1u64.to_le_bytes());
            payload.extend_from_slice(&json_n.to_le_bytes());
            payload.extend_from_slice(b"{}"); // only 2 bytes actually present
            let e = ControlClientMsg::from_frame(kind::REQUEST, payload.clone()).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "json_n {json_n}");
            let e = ControlServerMsg::from_frame(kind::RESPONSE_BLOB, payload).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "json_n {json_n}");
        }
    }

    /// A non-blob kind carrying trailing bytes is a desync, not something to
    /// tolerate: `12 + json_n == payload_len` is required for 61 and 63.
    #[test]
    fn trailing_bytes_on_a_non_blob_kind_are_invalid_data() {
        let json = serde_json::to_vec(&ControlRequest::Ping).unwrap();
        let mut payload = encode_body(1, &json, b"extra").unwrap();
        let e = ControlClientMsg::from_frame(kind::REQUEST, payload.clone()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);

        payload = encode_body(1, &[], b"extra").unwrap();
        let e = ControlClientMsg::from_frame(kind::CANCEL, payload).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);

        let json = serde_json::to_vec(&ControlEvent::WatchOverflow { id: 1 }).unwrap();
        let payload = encode_body(0, &json, b"extra").unwrap();
        let e = ControlServerMsg::from_frame(kind::EVENT, payload).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    /// An event with a non-zero `req_id` is a protocol error — a push has no
    /// request to belong to, and letting it through would let a server steal a
    /// pending reply's slot.
    #[test]
    fn event_with_a_nonzero_req_id_is_invalid_data() {
        let json = serde_json::to_vec(&ControlEvent::WatchOverflow { id: 1 }).unwrap();
        let payload = encode_body(9, &json, &[]).unwrap();
        let e = ControlServerMsg::from_frame(kind::EVENT, payload).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    /// Conversely, req_id 0 on anything that is *not* an event is a protocol
    /// error: 0 belongs to pushes.
    #[test]
    fn zero_req_id_is_rejected_on_requests_and_responses() {
        let json = serde_json::to_vec(&ControlRequest::Ping).unwrap();
        let payload = encode_body(0, &json, &[]).unwrap();
        for k in [kind::REQUEST, kind::CANCEL] {
            let e = ControlClientMsg::from_frame(k, payload.clone()).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "kind {k}");
        }
        let e = ControlClientMsg::from_frame(kind::REQUEST_BLOB, payload).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);

        let json = serde_json::to_vec(&ControlReply::Ok(ReplyOk::Unit)).unwrap();
        let payload = encode_body(0, &json, &[]).unwrap();
        for k in [kind::RESPONSE, kind::RESPONSE_BLOB] {
            let e = ControlServerMsg::from_frame(k, payload.clone()).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "kind {k}");
        }

        // And the encoder refuses to mint one in the first place.
        let mut buf = Vec::new();
        assert!(
            ControlClientMsg::Request {
                req_id: 0,
                req: ControlRequest::Ping
            }
            .encode(&mut buf)
            .is_err()
        );
    }

    /// Unknown kinds are errors, not skips — the same verdict the pane protocol
    /// reaches, and the reason control chose 60-63 rather than reusing the
    /// retired 13.
    #[test]
    fn unknown_kinds_are_invalid_data() {
        for k in [0u8, 1, 13, 40, 50, 59, 64, 69, 200, 255] {
            let e = ControlClientMsg::from_frame(k, vec![0; 32]).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "client kind {k}");
            let e = ControlServerMsg::from_frame(k, vec![0; 32]).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "server kind {k}");
        }
    }

    /// Control's kinds do not collide with anything the pane protocol uses or
    /// has reserved, and never touch the retired 13.
    #[test]
    fn control_kinds_sit_in_their_own_range() {
        let ours = [
            kind::HELLO,
            kind::REQUEST,
            kind::REQUEST_BLOB,
            kind::CANCEL,
            kind::HELLO_OK,
            kind::RESPONSE,
            kind::RESPONSE_BLOB,
            kind::EVENT,
        ];
        for k in ours {
            assert!((60..=63).contains(&k), "control kind {k} left its range");
            assert_ne!(k, 13, "13 is retired and must never be reused");
        }
        // Every kind the pane protocol uses or reserves, per protocol.rs.
        let taken: Vec<u8> = (1..=24).chain(30..=36).chain([40, 50]).collect();
        for k in ours {
            assert!(
                !taken.contains(&k),
                "control kind {k} collides with protocol"
            );
        }
    }

    /// Malformed JSON inside a well-formed frame is `InvalidData`, not a panic.
    #[test]
    fn malformed_json_is_invalid_data() {
        let payload = encode_body(1, b"{not json", &[]).unwrap();
        let e = ControlClientMsg::from_frame(kind::REQUEST, payload).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);

        let e = ControlClientMsg::from_frame(kind::HELLO, b"nope".to_vec()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    /// A frame past `MAX_FRAME` is refused at encode time rather than shipped
    /// for the peer to refuse — the writer is where the allocation already is.
    #[test]
    fn oversize_blob_is_refused_at_encode() {
        let blob = vec![0u8; MAX_FRAME];
        let mut sink = Vec::new();
        let e = ControlClientMsg::RequestBlob {
            req_id: 1,
            req: ControlRequest::WriteFile {
                path: "/tmp/huge".into(),
            },
            blob,
        }
        .encode(&mut sink)
        .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    /// And a *claimed* length past `MAX_FRAME` is refused at decode without
    /// allocating it.
    #[test]
    fn oversize_declared_length_is_refused_at_decode() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_FRAME + 1) as u32).to_le_bytes());
        buf.push(kind::REQUEST);
        let e = ControlClientMsg::read(&mut Cursor::new(&buf)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    /// A frame exactly at `MAX_FRAME` is legal, so the boundary is inclusive on
    /// both sides and a 64 MiB read isn't silently one byte short.
    #[test]
    fn a_frame_exactly_at_max_frame_survives() {
        let json = serde_json::to_vec(&ControlRequest::WriteFile {
            path: "/tmp/big".into(),
        })
        .unwrap();
        let blob = vec![0x5a; MAX_FRAME - CONTROL_HEADER - json.len()];
        let msg = ControlClientMsg::RequestBlob {
            req_id: 1,
            req: ControlRequest::WriteFile {
                path: "/tmp/big".into(),
            },
            blob,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        assert_eq!(
            u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize,
            MAX_FRAME
        );
        assert_eq!(ControlClientMsg::read(&mut Cursor::new(&buf)).unwrap(), msg);
    }

    // ---- error mapping ------------------------------------------------------

    /// The `io::ErrorKind` ↔ `WireErrorKind` tables are inverses on every kind
    /// they name. If they are not, a `NotFound` from the server becomes an
    /// `Other` on the client and every "file is missing" path in the GUI stops
    /// recognizing itself.
    #[test]
    fn error_kinds_round_trip_through_io() {
        use io::ErrorKind as K;
        let pairs = [
            (K::NotFound, WireErrorKind::NotFound),
            (K::PermissionDenied, WireErrorKind::PermissionDenied),
            (K::AlreadyExists, WireErrorKind::AlreadyExists),
            (K::InvalidInput, WireErrorKind::InvalidInput),
            (K::NotADirectory, WireErrorKind::NotADirectory),
            (K::IsADirectory, WireErrorKind::IsADirectory),
            (K::DirectoryNotEmpty, WireErrorKind::DirectoryNotEmpty),
            (K::FileTooLarge, WireErrorKind::FileTooLarge),
            (K::TimedOut, WireErrorKind::TimedOut),
            (K::ConnectionReset, WireErrorKind::ConnectionReset),
        ];
        for (io_kind, wire_kind) in pairs {
            let wire = WireError::from_io(&io::Error::new(io_kind, "boom"));
            assert_eq!(wire.kind, wire_kind, "io {io_kind:?} -> wire");
            assert_eq!(wire.into_io().kind(), io_kind, "wire {wire_kind:?} -> io");
        }
    }

    /// The kinds that collapse on the way out stay collapsed, deliberately.
    #[test]
    fn error_kinds_that_collapse_do_so_predictably() {
        use io::ErrorKind as K;
        for k in [K::BrokenPipe, K::UnexpectedEof, K::ConnectionReset] {
            assert_eq!(
                WireError::from_io(&io::Error::new(k, "x")).kind,
                WireErrorKind::ConnectionReset
            );
        }
        assert_eq!(
            WireError::from_io(&io::Error::new(K::InvalidFilename, "x")).kind,
            WireErrorKind::InvalidInput
        );
        assert_eq!(
            WireError::from_io(&io::Error::new(K::WouldBlock, "x")).kind,
            WireErrorKind::Other
        );
        // GitUnavailable has no io kind of its own; it reads as "not found",
        // which is what a missing binary is.
        assert_eq!(WireErrorKind::GitUnavailable.to_io_kind(), K::NotFound);
    }

    /// The server's message survives into the local `io::Error`, because that
    /// string is what the user is shown.
    #[test]
    fn wire_error_message_survives_into_io() {
        let e = WireError::new(WireErrorKind::NotFound, "/home/me/gone: no such file").into_io();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("/home/me/gone"));
    }

    /// A non-zero git exit is a *successful* reply. This is the invariant that
    /// keeps `git_status`'s `Option<String>` semantics intact behind the trait.
    #[test]
    fn a_nonzero_git_exit_is_ok_not_err() {
        let reply = ControlReply::Ok(ReplyOk::Output(Output {
            status: Some(128),
            stdout: Vec::new(),
            stderr: b"not a git repository".to_vec(),
        }));
        let mut buf = Vec::new();
        ControlServerMsg::Response { req_id: 1, reply }
            .encode(&mut buf)
            .unwrap();
        match ControlServerMsg::read(&mut Cursor::new(&buf)).unwrap() {
            ControlServerMsg::Response { reply, .. } => {
                let ok = reply
                    .into_result()
                    .expect("a non-zero exit is not an error");
                match ok {
                    ReplyOk::Output(o) => {
                        assert_eq!(o.status, Some(128));
                        assert!(!o.success());
                        assert_eq!(o.stderr_trimmed(), "not a git repository");
                    }
                    other => panic!("expected Output, got {other:?}"),
                }
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    /// `Output`'s byte fields ride as base64 strings, not JSON number arrays.
    /// The difference is ~1.33× versus ~4×; on a megabyte of `git diff` that is
    /// the difference between a snappy panel and a stalled one.
    #[test]
    fn output_bytes_ride_as_base64_not_a_number_array() {
        let out = Output {
            status: Some(0),
            stdout: vec![0u8; 3000],
            stderr: Vec::new(),
        };
        let json = serde_json::to_string(&out).unwrap();
        assert!(
            json.contains("\"stdout\":\"AAAA"),
            "stdout should be a base64 string, got: {}",
            &json[..60.min(json.len())]
        );
        assert!(!json.contains("[0,0,0"), "must not be a number array");
        // 3000 bytes -> 4000 base64 chars, versus ~6000 for a number array.
        assert!(json.len() < 4200, "base64 payload is {} bytes", json.len());
        assert_eq!(serde_json::from_str::<Output>(&json).unwrap(), out);
    }

    // ---- timeouts -----------------------------------------------------------

    /// Every request has a deadline, and the classes keep the contract's shape:
    /// metadata is quick, content is patient, git and search sit between.
    #[test]
    fn deadlines_match_the_contract_table() {
        use ControlRequest as R;
        let s = Duration::from_secs;
        let cases: Vec<(R, Duration)> = vec![
            (R::Ping, s(5)),
            (
                R::ReadDir {
                    dir: "/".into(),
                    root: None,
                },
                s(5),
            ),
            (
                R::ReadDir {
                    dir: "/".into(),
                    root: Some("/".into()),
                },
                s(5),
            ),
            (R::Stat { path: "/".into() }, s(5)),
            (R::Exists { path: "/".into() }, s(5)),
            (R::Canonicalize { path: "/".into() }, s(5)),
            (R::RepoRoot { path: "/".into() }, s(5)),
            (R::WatchOpen { dirs: vec![] }, s(5)),
            (
                R::WatchSet {
                    id: 1,
                    dirs: vec![],
                },
                s(5),
            ),
            (R::WatchClose { id: 1 }, s(5)),
            (
                R::ReadFile {
                    path: "/".into(),
                    max_bytes: 1,
                },
                s(30),
            ),
            (R::WriteFile { path: "/".into() }, s(30)),
            (R::CreateFileNew { path: "/".into() }, s(10)),
            (
                R::CreateDir {
                    path: "/".into(),
                    recursive: false,
                },
                s(10),
            ),
            (
                R::Rename {
                    from: "/".into(),
                    to: "/".into(),
                },
                s(10),
            ),
            (
                R::Remove {
                    path: "/".into(),
                    recursive: false,
                },
                s(10),
            ),
            (
                R::Git {
                    cwd: "/".into(),
                    args: vec![],
                },
                s(20),
            ),
            (
                R::Search {
                    roots: vec![],
                    query: String::new(),
                    limit: 1,
                    max_dirs: 1,
                    show_hidden: true,
                },
                s(20),
            ),
        ];
        assert_eq!(
            cases.len(),
            every_request().len(),
            "every request variant needs a deadline case"
        );
        for (req, want) in cases {
            assert_eq!(req.deadline(), want, "deadline for {req:?}");
        }
    }

    /// Only `WriteFile` takes a request blob; only `ReadFile` returns one. The
    /// client picks the frame kind from these, so a wrong answer here silently
    /// changes the wire.
    #[test]
    fn blob_shape_is_known_per_request() {
        for req in every_request() {
            let takes = matches!(req, ControlRequest::WriteFile { .. });
            let returns = matches!(req, ControlRequest::ReadFile { .. });
            assert_eq!(req.takes_blob(), takes, "takes_blob for {req:?}");
            assert_eq!(req.returns_blob(), returns, "returns_blob for {req:?}");
        }
    }

    /// The handshake carries the server's separator and home, which is how a
    /// Windows client ends up doing POSIX path arithmetic for a Linux host.
    #[test]
    fn hello_ok_round_trips_with_features() {
        let mut buf = Vec::new();
        ControlServerMsg::HelloOk(hello_ok())
            .encode(&mut buf)
            .unwrap();
        match ControlServerMsg::read(&mut Cursor::new(&buf)).unwrap() {
            ControlServerMsg::HelloOk(ok) => {
                assert_eq!(ok.separator, '/');
                assert_eq!(ok.home, "/home/me");
                assert!(ok.has_feature(feature::CONTROL));
                assert!(ok.has_feature(feature::HOST_RPC));
                // The retired record store's bit is a burned name and must
                // never come back.
                assert!(!ok.has_feature("workspace-store"));
            }
            other => panic!("expected HelloOk, got {other:?}"),
        }
    }

    /// `features` is `#[serde(default)]`, so a peer that predates the field
    /// still decodes — the whole reason capabilities are a list rather than
    /// another version number.
    #[test]
    fn hello_ok_without_features_still_decodes() {
        let json = br#"{"control_version":1,"protocol_version":3,"build":"old",
                        "separator":"/","home":"/root"}"#;
        let ok: ControlHelloOk = serde_json::from_slice(json).unwrap();
        assert!(ok.features.is_empty());
        assert!(!ok.has_feature(feature::CONTROL));
    }

    // ---- ControlClient over a real duplex stream ----------------------------
    //
    // These drive a genuine loopback TCP connection with a scripted peer on the
    // far thread, rather than a `Cursor`. A `Cursor` can prove the codec is
    // symmetric; it cannot prove that a reply is claimed by the right waiter,
    // that a slow request doesn't hold up a fast one, or that a dead socket
    // wakes everyone. Those are properties of the *client*, and they only exist
    // once there are two threads and a real stream between them.

    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    /// Stand up a scripted peer on loopback and connect a client to it.
    ///
    /// `serve` receives the peer's own socket after the handshake has been
    /// answered, and drives whatever exchange the test needs.
    fn client_with_peer<F>(events: EventSink, serve: F) -> ControlClient
    where
        F: FnOnce(TcpStream) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Hello(h) => assert_eq!(h.control_version, CONTROL_VERSION),
                other => panic!("expected Hello, got {other:?}"),
            }
            ControlServerMsg::HelloOk(hello_ok())
                .encode(&mut sock)
                .unwrap();
            sock.flush().unwrap();
            serve(sock);
        });
        let sock = TcpStream::connect(addr).unwrap();
        ControlClient::over_tcp(sock, &hello(), events).unwrap()
    }

    fn no_events() -> EventSink {
        Box::new(|_| {})
    }

    fn reply_to<W: Write>(w: &mut W, req_id: u64, reply: ReplyOk) {
        ControlServerMsg::Response {
            req_id,
            reply: ControlReply::Ok(reply),
        }
        .encode(w)
        .unwrap();
        w.flush().unwrap();
    }

    /// The handshake completes, a request goes out, and its reply comes back —
    /// over loopback TCP, with the client's reader thread doing the decoding.
    #[test]
    fn a_request_and_its_reply_cross_a_real_duplex_stream() {
        let client = client_with_peer(no_events(), |mut sock| {
            for _ in 0..3 {
                match ControlClientMsg::read(&mut sock) {
                    Ok(ControlClientMsg::Request { req_id, req }) => {
                        let reply = match req {
                            ControlRequest::Ping => ReplyOk::Pong,
                            ControlRequest::Stat { .. } => ReplyOk::Meta(meta()),
                            ControlRequest::Exists { .. } => ReplyOk::Bool(true),
                            other => panic!("unexpected request {other:?}"),
                        };
                        reply_to(&mut sock, req_id, reply);
                    }
                    other => panic!("unexpected message {other:?}"),
                }
            }
        });

        assert_eq!(client.hello().separator, '/');
        assert_eq!(client.hello().home, "/home/me");
        assert!(client.is_connected());

        assert_eq!(client.call(ControlRequest::Ping).unwrap(), ReplyOk::Pong);
        assert_eq!(
            client
                .call(ControlRequest::Stat {
                    path: "/etc/hosts".into()
                })
                .unwrap(),
            ReplyOk::Meta(meta())
        );
        assert_eq!(
            client
                .call(ControlRequest::Exists {
                    path: "/etc".into()
                })
                .unwrap(),
            ReplyOk::Bool(true)
        );
    }

    /// **The reason this layer exists.** A slow request must not hold up a fast
    /// one issued behind it.
    ///
    /// The peer here deliberately answers out of order: it parks a `Git` that
    /// arrived first, answers a `ReadDir` that arrived second, and only then
    /// goes back to the `Git`. If replies were matched by arrival order — or if
    /// one shared lock serialized callers — the `ReadDir` would sit behind the
    /// `Git` and the file tree would freeze every time the status bar asked a
    /// slow question. The completion order recorded below is the proof it does
    /// not.
    #[test]
    fn a_slow_request_does_not_block_a_fast_one_behind_it() {
        let client = Arc::new(client_with_peer(no_events(), |mut sock| {
            let mut parked_git = None;
            loop {
                match ControlClientMsg::read(&mut sock).unwrap() {
                    ControlClientMsg::Request { req_id, req } => match req {
                        ControlRequest::Git { .. } => parked_git = Some(req_id),
                        ControlRequest::ReadDir { .. } => {
                            reply_to(&mut sock, req_id, ReplyOk::Entries(vec![]));
                            // Only now, well after the fast reply, does the
                            // slow one land.
                            thread::sleep(Duration::from_millis(250));
                            reply_to(
                                &mut sock,
                                parked_git.expect("git arrived first"),
                                ReplyOk::Output(Output {
                                    status: Some(0),
                                    stdout: b"clean".to_vec(),
                                    stderr: Vec::new(),
                                }),
                            );
                            return;
                        }
                        other => panic!("unexpected request {other:?}"),
                    },
                    other => panic!("unexpected message {other:?}"),
                }
            }
        }));

        let (done_tx, done_rx) = mpsc::channel::<&'static str>();

        let slow_client = Arc::clone(&client);
        let slow_done = done_tx.clone();
        let slow = thread::spawn(move || {
            let out = slow_client
                .call(ControlRequest::Git {
                    cwd: "/repo".into(),
                    args: vec!["status".into()],
                })
                .unwrap();
            slow_done.send("git").unwrap();
            out
        });

        // Let the slow request reach the peer first, so it is unambiguously
        // *ahead* of the fast one in both issue order and req_id.
        thread::sleep(Duration::from_millis(100));
        let entries = client
            .call(ControlRequest::ReadDir {
                dir: "/repo".into(),
                root: None,
            })
            .unwrap();
        done_tx.send("read_dir").unwrap();

        assert_eq!(entries, ReplyOk::Entries(vec![]));
        assert_eq!(
            done_rx.recv().unwrap(),
            "read_dir",
            "the fast request must finish first even though it was issued second"
        );
        assert_eq!(done_rx.recv().unwrap(), "git");
        assert!(matches!(
            slow.join().unwrap(),
            ReplyOk::Output(o) if o.stdout_trimmed() == "clean"
        ));
    }

    /// Many concurrent callers, each answered with a value only they asked for,
    /// with the peer replying in reverse order. Proves the pending table keys
    /// on `req_id` rather than on anything positional.
    #[test]
    fn concurrent_callers_each_get_their_own_reply() {
        const N: u64 = 16;
        let client = Arc::new(client_with_peer(no_events(), |mut sock| {
            let mut seen = Vec::new();
            while (seen.len() as u64) < N {
                match ControlClientMsg::read(&mut sock).unwrap() {
                    ControlClientMsg::Request { req_id, req } => seen.push((req_id, req)),
                    other => panic!("unexpected message {other:?}"),
                }
            }
            // Reverse order, and each reply echoes the *request's* own path so
            // a mismatched delivery is visible rather than merely plausible.
            for (req_id, req) in seen.into_iter().rev() {
                let path = match req {
                    ControlRequest::Canonicalize { path } => path,
                    other => panic!("unexpected request {other:?}"),
                };
                reply_to(&mut sock, req_id, ReplyOk::Path(path));
            }
        }));

        let handles: Vec<_> = (0..N)
            .map(|i| {
                let c = Arc::clone(&client);
                thread::spawn(move || {
                    let want = format!("/p/{i}");
                    let got = c
                        .call(ControlRequest::Canonicalize { path: want.clone() })
                        .unwrap();
                    assert_eq!(
                        got,
                        ReplyOk::Path(want),
                        "caller {i} received another caller's reply"
                    );
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    /// A request that outruns its deadline fails with `TimedOut`, sends a
    /// `CONTROL_CANCEL`, and — critically — **does not drop the connection**:
    /// the next request on the same client still works.
    #[test]
    fn a_timeout_cancels_that_request_and_leaves_the_connection_usable() {
        let (saw_cancel_tx, saw_cancel_rx) = mpsc::channel();
        let client = client_with_peer(no_events(), move |mut sock| {
            // Swallow the first request without answering it.
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Request { .. } => {}
                other => panic!("unexpected message {other:?}"),
            }
            // The client should now give up and cancel.
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Cancel { req_id } => saw_cancel_tx.send(req_id).unwrap(),
                other => panic!("expected Cancel, got {other:?}"),
            }
            // A late reply to the abandoned request: it must be discarded, not
            // handed to whoever asks next.
            reply_to(&mut sock, 1, ReplyOk::Path("/late".into()));
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Request { req_id, .. } => {
                    reply_to(&mut sock, req_id, ReplyOk::Pong)
                }
                other => panic!("unexpected message {other:?}"),
            }
        });

        let e = client
            .call_with_deadline(
                ControlRequest::Canonicalize {
                    path: "/slow".into(),
                },
                &[],
                Duration::from_millis(150),
            )
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert_eq!(saw_cancel_rx.recv().unwrap(), 1, "cancel names the request");

        assert!(client.is_connected(), "a timeout must not kill the link");
        assert_eq!(
            client.call(ControlRequest::Ping).unwrap(),
            ReplyOk::Pong,
            "the late reply to the abandoned request must not have been \
             mistaken for this one's"
        );
    }

    /// When the link dies, every waiting caller is woken with
    /// `ConnectionReset` immediately — not left to serve out its own deadline,
    /// which for a `ReadFile` would be thirty seconds of a frozen editor.
    #[test]
    fn losing_the_link_fails_in_flight_requests_at_once() {
        let client = Arc::new(client_with_peer(no_events(), |mut sock| {
            // Take one request, then hang up without answering.
            let _ = ControlClientMsg::read(&mut sock);
            drop(sock);
        }));

        let started = Instant::now();
        let e = client
            .call(ControlRequest::ReadFile {
                path: "/big".into(),
                max_bytes: u64::MAX,
            })
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionReset);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "should fail on the hangup, not wait out the 30s ReadFile deadline"
        );
        assert!(!client.is_connected());

        // And a request issued afterwards fails immediately rather than
        // queueing against a dead socket.
        let e = client.call(ControlRequest::Ping).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionReset);
    }

    /// Bulk bytes ride both directions: content up with `WriteFile`, content
    /// down with `ReadFile`, each beside a JSON head carrying the parameters
    /// that a bare-blob frame could not have held.
    #[test]
    fn blobs_ride_in_both_directions() {
        let content: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
        let echoed = content.clone();
        let client = client_with_peer(no_events(), move |mut sock| {
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::RequestBlob { req_id, req, blob } => {
                    assert_eq!(
                        req,
                        ControlRequest::WriteFile {
                            path: "/tmp/f".into()
                        }
                    );
                    assert_eq!(blob, echoed);
                    reply_to(&mut sock, req_id, ReplyOk::Meta(meta()));
                }
                other => panic!("expected RequestBlob, got {other:?}"),
            }
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Request { req_id, .. } => {
                    ControlServerMsg::ResponseBlob {
                        req_id,
                        reply: ControlReply::Ok(ReplyOk::FileMeta { meta: meta() }),
                        blob: echoed.clone(),
                    }
                    .encode(&mut sock)
                    .unwrap();
                    sock.flush().unwrap();
                }
                other => panic!("expected Request, got {other:?}"),
            }
        });

        let wrote = client
            .call_with_blob(
                ControlRequest::WriteFile {
                    path: "/tmp/f".into(),
                },
                &content,
            )
            .unwrap();
        assert_eq!(wrote, ReplyOk::Meta(meta()));

        let read = client
            .call_full(
                ControlRequest::ReadFile {
                    path: "/tmp/f".into(),
                    max_bytes: u64::MAX,
                },
                &[],
            )
            .unwrap();
        assert_eq!(read.reply, ReplyOk::FileMeta { meta: meta() });
        assert_eq!(read.blob, content, "the blob is the file's content");
    }

    /// An error reply becomes an `io::Error` of the matching kind, carrying the
    /// server's message — this is the path every "file is gone" notification in
    /// the GUI takes.
    #[test]
    fn an_error_reply_becomes_a_matching_io_error() {
        let client = client_with_peer(no_events(), |mut sock| {
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Request { req_id, .. } => {
                    ControlServerMsg::Response {
                        req_id,
                        reply: ControlReply::Err(WireError::new(
                            WireErrorKind::FileTooLarge,
                            "/big is 900 MB, over the 10 MB limit",
                        )),
                    }
                    .encode(&mut sock)
                    .unwrap();
                    sock.flush().unwrap();
                }
                other => panic!("unexpected message {other:?}"),
            }
        });

        let e = client
            .call(ControlRequest::ReadFile {
                path: "/big".into(),
                max_bytes: 10 * 1024 * 1024,
            })
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::FileTooLarge);
        assert!(e.to_string().contains("900 MB"));
    }

    /// Pushes reach the sink without any request to hang them on, and interleave
    /// freely with replies rather than being serialized behind them.
    #[test]
    fn events_reach_the_sink_interleaved_with_replies() {
        let (tx, rx) = mpsc::channel();
        let sink: EventSink = Box::new(move |e| tx.send(e).unwrap());
        let client = client_with_peer(sink, |mut sock| {
            ControlServerMsg::Event(ControlEvent::Watch {
                id: 1,
                paths: vec!["/a".into()],
            })
            .encode(&mut sock)
            .unwrap();
            match ControlClientMsg::read(&mut sock).unwrap() {
                ControlClientMsg::Request { req_id, .. } => {
                    ControlServerMsg::Event(ControlEvent::WatchOverflow { id: 1 })
                        .encode(&mut sock)
                        .unwrap();
                    reply_to(&mut sock, req_id, ReplyOk::Pong);
                }
                other => panic!("unexpected message {other:?}"),
            }
            ControlServerMsg::Event(ControlEvent::Preempted {
                workspace: "w1".into(),
                by: "other".into(),
            })
            .encode(&mut sock)
            .unwrap();
            sock.flush().unwrap();
        });

        assert_eq!(client.call(ControlRequest::Ping).unwrap(), ReplyOk::Pong);
        let mut got = Vec::new();
        for _ in 0..3 {
            got.push(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        }
        assert_eq!(
            got,
            vec![
                ControlEvent::Watch {
                    id: 1,
                    paths: vec!["/a".into()]
                },
                ControlEvent::WatchOverflow { id: 1 },
                ControlEvent::Preempted {
                    workspace: "w1".into(),
                    by: "other".into()
                },
            ]
        );
    }

    /// Request ids start at 1 and never reissue 0, which is what keeps a push
    /// from ever being mistaken for a reply.
    #[test]
    fn request_ids_start_at_one_and_increase() {
        let (tx, rx) = mpsc::channel();
        let client = client_with_peer(no_events(), move |mut sock| {
            for _ in 0..4 {
                match ControlClientMsg::read(&mut sock).unwrap() {
                    ControlClientMsg::Request { req_id, .. } => {
                        tx.send(req_id).unwrap();
                        reply_to(&mut sock, req_id, ReplyOk::Pong);
                    }
                    other => panic!("unexpected message {other:?}"),
                }
            }
        });
        for _ in 0..4 {
            client.call(ControlRequest::Ping).unwrap();
        }
        let ids: Vec<u64> = (0..4).map(|_| rx.recv().unwrap()).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }

    /// A peer speaking a different control dialect is reported as exactly that,
    /// naming both versions. The peer answers the handshake and *then* hangs up
    /// — a `HELLO` has no `req_id`, so there is no error reply to carry the
    /// mismatch, and without this frame the client would only see a closed
    /// socket.
    #[test]
    fn a_control_version_mismatch_names_both_versions() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = ControlClientMsg::read(&mut sock);
            let mut ok = hello_ok();
            ok.control_version = CONTROL_VERSION + 7;
            ok.build = "from-the-future".into();
            ControlServerMsg::HelloOk(ok).encode(&mut sock).unwrap();
            sock.flush().unwrap();
        });

        let sock = TcpStream::connect(addr).unwrap();
        let e = ControlClient::over_tcp(sock, &hello(), no_events()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        let msg = e.to_string();
        assert!(msg.contains("from-the-future"), "names the peer: {msg}");
        assert!(
            msg.contains(&format!("v{}", CONTROL_VERSION + 7)),
            "names their version: {msg}"
        );
        assert!(
            msg.contains(&format!("v{CONTROL_VERSION}")),
            "names ours: {msg}"
        );
        assert!(
            is_dialect_refusal(&msg),
            "and the GUI can tell this apart from an unreachable host, which is what \
             decides whether it may offer to replace the far end's binary: {msg}"
        );
    }

    /// **Only a dialect refusal is recognised as one.**
    ///
    /// [`is_dialect_refusal`] gates a destructive offer — reinstalling the
    /// server on someone's machine and dropping every pane on it. A connect that
    /// failed for any other reason must never reach that button, because
    /// replacing the binary would not fix it and would cost the user their
    /// sessions to find out.
    #[test]
    fn other_connect_failures_are_not_dialect_refusals() {
        for other in [
            "Connection refused (os error 61)",
            "ssh: handshake failed: no matching key exchange method",
            "the remote tty7-server did not start: exit status 127",
            "could not resolve the remote home directory: Permission denied",
            "control peer answered the handshake with Bye instead of HELLO_OK",
        ] {
            assert!(!is_dialect_refusal(other), "{other}");
        }
        assert!(is_dialect_refusal(&dialect_refusal("26.7.6", 2, 3)));
    }

    /// A peer that answers the handshake with something other than `HELLO_OK`
    /// is a desync, not a greeting to be tolerated.
    #[test]
    fn a_handshake_answered_with_the_wrong_frame_is_invalid_data() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = ControlClientMsg::read(&mut sock);
            ControlServerMsg::Event(ControlEvent::WatchOverflow { id: 1 })
                .encode(&mut sock)
                .unwrap();
            sock.flush().unwrap();
        });
        let sock = TcpStream::connect(addr).unwrap();
        let e = ControlClient::over_tcp(sock, &hello(), no_events()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    /// **Regression: closing an idle client must not hang.**
    ///
    /// The reader spends its life blocked in `read_frame`, and an idle peer has
    /// no reason to send anything. Failing the pending table does not wake a
    /// thread that is inside a syscall, so a `close` that merely set a flag and
    /// then joined would wait for a frame neither side will ever send — and
    /// because `Drop` calls `close`, *every* dropped client would freeze
    /// whichever thread dropped it. For a user shutting a remote workspace that
    /// is the UI thread.
    #[test]
    fn closing_an_idle_client_returns_promptly() {
        let client = client_with_peer(no_events(), |mut sock| {
            // A peer that answers nothing and never hangs up: exactly the
            // situation a healthy, quiet connection is in.
            let _ = ControlClientMsg::read(&mut sock);
            thread::sleep(Duration::from_secs(30));
        });
        assert_eq!(client.call(ControlRequest::Ping).unwrap_err().kind(), {
            // The peer never replies, so this times out — leaving the reader
            // parked exactly where the bug needs it.
            io::ErrorKind::TimedOut
        });

        let started = Instant::now();
        client.close();
        assert!(
            started.elapsed() < CLOSE_GRACE * 2,
            "close() took {:?}; it must not wait on a peer that will never speak",
            started.elapsed()
        );
    }

    /// And the same guarantee through `Drop`, which is how it is actually
    /// reached in production — nobody calls `close` by hand.
    #[test]
    fn dropping_an_idle_client_returns_promptly() {
        let client = client_with_peer(no_events(), |mut sock| {
            let _ = ControlClientMsg::read(&mut sock);
            thread::sleep(Duration::from_secs(30));
        });
        let started = Instant::now();
        drop(client);
        assert!(
            started.elapsed() < CLOSE_GRACE * 2,
            "drop took {:?}",
            started.elapsed()
        );
    }

    /// With a shutdown wired up the reader is genuinely reaped, not merely
    /// abandoned — the fast path, and the one that keeps threads from piling up
    /// across a session's worth of reconnects.
    #[test]
    fn close_reaps_the_reader_when_the_link_can_be_shut_down() {
        let client = client_with_peer(no_events(), |mut sock| {
            let _ = ControlClientMsg::read(&mut sock);
            thread::sleep(Duration::from_secs(30));
        });
        let started = Instant::now();
        client.close();
        // A shut-down socket wakes the reader in microseconds; anything near
        // the grace period would mean it was detached rather than reaped.
        assert!(
            started.elapsed() < CLOSE_GRACE,
            "reader should have been reaped, not waited out: {:?}",
            started.elapsed()
        );
        assert!(!client.is_connected());
    }

    /// A frame the client cannot parse drops the connection rather than being
    /// skipped: once the stream's position is in doubt, nothing after it can be
    /// trusted. Waiting callers are woken, not left hanging.
    #[test]
    fn a_malformed_frame_tears_the_connection_down() {
        let client = client_with_peer(no_events(), |mut sock| {
            let _ = ControlClientMsg::read(&mut sock);
            // A well-framed frame of an unknown control kind.
            write_frame(&mut sock, 64, &[0u8; 16]).unwrap();
            sock.flush().unwrap();
            thread::sleep(Duration::from_secs(2));
        });
        let e = client.call(ControlRequest::Ping).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionReset);
        assert!(!client.is_connected());
    }
}
